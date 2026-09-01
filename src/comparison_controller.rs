//! Control-plane bridge from persistent comparisons to the shared audition.
//!
//! A comparison channel is immutable, project-frame-aligned PCM.  This module
//! validates that identity and turns it into a [`TimelineAudition`]; it never
//! owns a transport, audio device, renderer, or mutable DSP graph.  `Excess`
//! remains a coverage field rather than inventing an unspecified time-domain
//! signal merely to make every UI tab audible.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::audio::{AudioFormat, ProjectAudio};
use crate::change_set::{BusImpact, ChangeSet};
use crate::comparison::{
    ComparisonDefinition, ComparisonId, ComparisonObservation, ExactRenderDigest,
};
use crate::comparison_runtime::{
    exact_audio_digest, ComparisonRenderProducts, COMPARISON_CONSTRUCTION_SCOPE_NAMESPACE,
    COMPARISON_RESIDUAL_SCOPE_NAMESPACE, COMPARISON_SOURCE_SCOPE_NAMESPACE,
};
use crate::daw_project::{ProjectDomain, ProjectRevisions};
use crate::project_audio_controller::{
    AuditionAlignment, ProjectAudioController, ProjectAudioControllerError,
};
use crate::project_session::{ProjectAudioStatus, ProjectPublication, ScopedAuditionPhase};
use crate::render_plan::{
    ExactDigest, ExplanationScopeId, ProjectRevisionStamp, RenderFormat, RenderPlanId, RenderScope,
    RenderSpan,
};
use crate::render_products::{ProductPartition, RenderProduct};
use crate::render_runtime::{
    canonical_pcm_digest, AuditionMix, AuditionOwner, AuditionSubject, RuntimeRenderedAudio,
    TimelineAudition, TimelineAuditionId,
};
use crate::{audio_host::AudioHost, render_runtime::RenderRuntimeError};

pub const COMPARISON_AUDITION_OWNER_NAMESPACE: u128 = u128::from_be_bytes(*b"audec-compare-v1");

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ComparisonChannel {
    Source,
    Construction,
    Residual,
    /// Time-frequency over-explanation. There is deliberately no PCM until a
    /// separately specified inverse/sonification recipe exists.
    Excess,
}

impl ComparisonChannel {
    const fn subject(self) -> AuditionSubject {
        match self {
            Self::Source => AuditionSubject::Source,
            Self::Construction => AuditionSubject::Construction,
            Self::Residual => AuditionSubject::Residual,
            Self::Excess => AuditionSubject::Excess,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComparisonDigestPins {
    pub source: ExactRenderDigest,
    pub construction: ExactRenderDigest,
    pub residual: ExactRenderDigest,
}

impl From<&ComparisonObservation> for ComparisonDigestPins {
    fn from(observation: &ComparisonObservation) -> Self {
        Self {
            source: observation.source_digest,
            construction: observation.construction_digest,
            residual: observation.residual_digest,
        }
    }
}

/// A selection token crosses worker boundaries. Completion is accepted only
/// if this exact generation is still desired, so a slow source render cannot
/// replace a newer residual selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonSelectionRequest {
    pub generation: u64,
    pub comparison: ComparisonId,
    pub explanation: crate::explanation::ExplanationId,
    pub channel: ComparisonChannel,
    pub span: RenderSpan,
    pub requested_at: ProjectRevisions,
    pub digests: ComparisonDigestPins,
    dependencies: crate::explanation::ExplanationDependencyPin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComparisonControllerPhase {
    Idle,
    AwaitingProducts,
    Ready,
    Publishing,
    Active,
    /// Selected and displayable in coverage, but not a time-domain signal.
    CoverageOnly,
    Stale(ComparisonInvalidation),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonControllerStatus {
    pub owner: AuditionOwner,
    pub selection: Option<ComparisonSelectionRequest>,
    pub phase: ComparisonControllerPhase,
    pub producing_revision: Option<ProjectRevisionStamp>,
    pub pcm_digest: Option<ExactDigest>,
    pub audition: Option<TimelineAuditionId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComparisonInvalidation {
    pub publication_generation: u64,
    pub revision: u64,
    pub reasons: Vec<ComparisonInvalidationReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComparisonInvalidationReason {
    SourceAssets,
    ExplanationDependency(ProjectDomain),
    Routing,
    AudioOverlap,
}

#[derive(Clone, Debug)]
pub enum ComparisonAudioEffect {
    Publish {
        generation: u64,
        audition: Arc<TimelineAudition>,
    },
    /// Selecting coverage-only excess clears this owner's old PCM but leaves
    /// the one project transport untouched.
    Clear {
        generation: u64,
        owner: AuditionOwner,
    },
}

impl ComparisonAudioEffect {
    pub const fn generation(&self) -> u64 {
        match self {
            Self::Publish { generation, .. } | Self::Clear { generation, .. } => *generation,
        }
    }
}

/// Validated, mutually aligned products from one comparison execution.
#[derive(Clone, Debug)]
pub struct ComparisonAuditionProducts {
    pub comparison: ComparisonId,
    pub explanation: crate::explanation::ExplanationId,
    pub span: RenderSpan,
    pub format: RenderFormat,
    pub producing_plan: RenderPlanId,
    pub observation: ComparisonObservation,
    source: Arc<RenderProduct>,
    construction: Arc<RenderProduct>,
    residual: Arc<RenderProduct>,
}

impl ComparisonAuditionProducts {
    pub fn validate(
        definition: &ComparisonDefinition,
        observation: ComparisonObservation,
        products: ComparisonRenderProducts,
    ) -> Result<Self, ComparisonControllerError> {
        definition
            .validate()
            .map_err(|error| ComparisonControllerError::Definition(error.to_string()))?;
        let span = RenderSpan::new(
            definition.source.project_span.start,
            definition.source.project_span.end,
        )
        .map_err(|error| ComparisonControllerError::Products(error.to_string()))?;
        let plan = products.source.produced_by.plan.clone();
        let format = products.source.id.format;
        validate_product(
            &products.source,
            &plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_SOURCE_SCOPE_NAMESPACE,
                local: definition.id.0,
            }),
            span,
            observation.source_digest,
        )?;
        validate_product(
            &products.construction,
            &plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_CONSTRUCTION_SCOPE_NAMESPACE,
                local: definition.explanation.0,
            }),
            span,
            observation.construction_digest,
        )?;
        validate_product(
            &products.residual,
            &plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_RESIDUAL_SCOPE_NAMESPACE,
                local: definition.id.0,
            }),
            span,
            observation.residual_digest,
        )?;
        for product in [&products.construction, &products.residual] {
            if product.id.format != format {
                return Err(ComparisonControllerError::ProductFormatMismatch);
            }
        }
        for (domain, generation) in &observation.dependencies.project {
            let actual = stamped_domain(plan.revisions, *domain);
            if actual != *generation {
                return Err(ComparisonControllerError::DependencyRevisionMismatch {
                    domain: *domain,
                    observation: *generation,
                    product: actual,
                });
            }
        }
        Ok(Self {
            comparison: definition.id,
            explanation: definition.explanation,
            span,
            format,
            producing_plan: plan,
            observation,
            source: products.source,
            construction: products.construction,
            residual: products.residual,
        })
    }

    /// The exact immutable signal for a time-domain channel. Spectral excess
    /// intentionally has no PCM product.
    pub fn product(&self, channel: ComparisonChannel) -> Option<&Arc<RenderProduct>> {
        match channel {
            ComparisonChannel::Source => Some(&self.source),
            ComparisonChannel::Construction => Some(&self.construction),
            ComparisonChannel::Residual => Some(&self.residual),
            ComparisonChannel::Excess => None,
        }
    }
}

fn validate_product(
    product: &RenderProduct,
    plan: &RenderPlanId,
    scope: RenderScope,
    span: RenderSpan,
    aligned_digest: ExactRenderDigest,
) -> Result<(), ComparisonControllerError> {
    if &product.produced_by.plan != plan {
        return Err(ComparisonControllerError::MixedPlans);
    }
    if product.produced_by.scope != scope {
        return Err(ComparisonControllerError::UnexpectedScope {
            expected: scope,
            actual: product.produced_by.scope.clone(),
        });
    }
    if product.produced_by.core != span {
        return Err(ComparisonControllerError::ProductSpanMismatch {
            expected: span,
            actual: product.produced_by.core,
        });
    }
    if !matches!(
        product.produced_by.partition,
        ProductPartition::ContiguousRun {
            anchor_frame,
            sequence: 0
        } if anchor_frame == span.start
    ) {
        return Err(ComparisonControllerError::UnexpectedPartition);
    }
    let pcm_digest = canonical_pcm_digest(product.interleaved());
    if pcm_digest != product.id.pcm {
        return Err(ComparisonControllerError::PcmDigestMismatch {
            recorded: product.id.pcm,
            actual: pcm_digest,
        });
    }
    let format = AudioFormat::new(
        product.id.format.sample_rate.get(),
        product.id.format.channels.get(),
    )
    .map_err(|error| ComparisonControllerError::Audio(error.to_string()))?;
    let audio = ProjectAudio::new(format, product.shared_interleaved())
        .map_err(|error| ComparisonControllerError::Audio(error.to_string()))?;
    let actual = exact_audio_digest(&audio, span.start)
        .map_err(|error| ComparisonControllerError::Products(error.to_string()))?;
    if actual != aligned_digest {
        return Err(ComparisonControllerError::AlignedDigestMismatch {
            recorded: aligned_digest,
            actual,
        });
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ReadyChannel {
    generation: u64,
    product: Arc<RenderProduct>,
    audition: Arc<TimelineAudition>,
}

pub struct ComparisonController {
    owner: AuditionOwner,
    next_generation: u64,
    selection: Option<ComparisonSelectionRequest>,
    phase: ComparisonControllerPhase,
    ready: Option<ReadyChannel>,
    producing_revision: Option<ProjectRevisionStamp>,
}

impl ComparisonController {
    pub fn new(owner_local: u64) -> Result<Self, ComparisonControllerError> {
        if owner_local == 0 {
            return Err(ComparisonControllerError::ZeroOwner);
        }
        Ok(Self {
            owner: AuditionOwner {
                namespace: COMPARISON_AUDITION_OWNER_NAMESPACE,
                local: owner_local,
            },
            next_generation: 1,
            selection: None,
            phase: ComparisonControllerPhase::Idle,
            ready: None,
            producing_revision: None,
        })
    }

    pub const fn owner(&self) -> AuditionOwner {
        self.owner
    }

    pub fn select(
        &mut self,
        definition: &ComparisonDefinition,
        observation: &ComparisonObservation,
        current_revisions: ProjectRevisions,
        channel: ComparisonChannel,
    ) -> Result<ComparisonSelectionRequest, ComparisonControllerError> {
        definition
            .validate()
            .map_err(|error| ComparisonControllerError::Definition(error.to_string()))?;
        let span = RenderSpan::new(
            definition.source.project_span.start,
            definition.source.project_span.end,
        )
        .map_err(|error| ComparisonControllerError::Definition(error.to_string()))?;
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(ComparisonControllerError::GenerationExhausted)?;
        let request = ComparisonSelectionRequest {
            generation,
            comparison: definition.id,
            explanation: definition.explanation,
            channel,
            span,
            requested_at: current_revisions,
            digests: observation.into(),
            dependencies: observation.dependencies.clone(),
        };
        self.ready = None;
        self.producing_revision = None;
        self.phase = stale_dependencies(&request.dependencies, current_revisions).map_or(
            ComparisonControllerPhase::AwaitingProducts,
            |reason| {
                ComparisonControllerPhase::Stale(ComparisonInvalidation {
                    publication_generation: 0,
                    revision: current_revisions.aggregate,
                    reasons: vec![reason],
                })
            },
        );
        self.selection = Some(request.clone());
        Ok(request)
    }

    pub fn accept_products(
        &mut self,
        request: &ComparisonSelectionRequest,
        products: Arc<ComparisonAuditionProducts>,
    ) -> Result<ComparisonAudioEffect, ComparisonControllerError> {
        self.require_current(request)?;
        if matches!(self.phase, ComparisonControllerPhase::Stale(_)) {
            return Err(ComparisonControllerError::StaleSelection(
                request.generation,
            ));
        }
        if products.comparison != request.comparison
            || products.explanation != request.explanation
            || products.span != request.span
        {
            return Err(ComparisonControllerError::ProductsDoNotMatchSelection);
        }
        if ComparisonDigestPins::from(&products.observation) != request.digests
            || products.observation.dependencies != request.dependencies
        {
            return Err(ComparisonControllerError::ObservationChanged);
        }
        if products.producing_plan.revisions.assets != request.requested_at.assets {
            return Err(ComparisonControllerError::SourceAssetRevisionMismatch {
                requested: request.requested_at.assets,
                product: products.producing_plan.revisions.assets,
            });
        }
        if let Some(reason) = stale_dependencies(&request.dependencies, request.requested_at) {
            return Err(ComparisonControllerError::RequestWasAlreadyStale(reason));
        }
        self.producing_revision = Some(products.producing_plan.revisions);
        let Some(product) = products.product(request.channel).cloned() else {
            self.ready = None;
            self.phase = ComparisonControllerPhase::CoverageOnly;
            return Ok(ComparisonAudioEffect::Clear {
                generation: request.generation,
                owner: self.owner,
            });
        };
        let audition = Arc::new(TimelineAudition::new(
            TimelineAuditionId {
                owner: self.owner,
                revision: request.generation,
                content: product.id.pcm,
            },
            request.channel.subject(),
            AuditionMix::Replace,
            request.span,
            products.format,
            product.shared_interleaved(),
        )?);
        self.ready = Some(ReadyChannel {
            generation: request.generation,
            product,
            audition: Arc::clone(&audition),
        });
        self.phase = ComparisonControllerPhase::Ready;
        Ok(ComparisonAudioEffect::Publish {
            generation: request.generation,
            audition,
        })
    }

    /// Record a worker/decoder/compiler failure only if it belongs to the
    /// current request. A late failure cannot erase a newer ready selection.
    pub fn fail_request(
        &mut self,
        request: &ComparisonSelectionRequest,
        message: impl Into<String>,
    ) -> Result<(), ComparisonControllerError> {
        self.require_current(request)?;
        self.ready = None;
        self.producing_revision = None;
        self.phase = ComparisonControllerPhase::Failed(message.into());
        Ok(())
    }

    pub fn apply_audio_effect(
        &mut self,
        audio: &mut ProjectAudioController,
        host: &AudioHost,
        effect: ComparisonAudioEffect,
        alignment: AuditionAlignment,
    ) -> Result<(), ComparisonControllerError> {
        let desired = self
            .selection
            .as_ref()
            .ok_or(ComparisonControllerError::NoSelection)?;
        if effect.generation() != desired.generation {
            return Err(ComparisonControllerError::ObsoleteEffect {
                completed: effect.generation(),
                desired: desired.generation,
            });
        }
        match effect {
            ComparisonAudioEffect::Publish { audition, .. } => {
                audio.start_scoped_audition(host, audition, alignment)?;
                self.phase = ComparisonControllerPhase::Publishing;
            }
            ComparisonAudioEffect::Clear { owner, .. } => {
                audio.stop_scoped_audition(owner)?;
                self.phase = ComparisonControllerPhase::CoverageOnly;
            }
        }
        Ok(())
    }

    /// Reconcile cheap audio receipts into comparison-strip status.
    pub fn observe_audio_status(&mut self, status: &ProjectAudioStatus) {
        if matches!(self.phase, ComparisonControllerPhase::Stale(_)) {
            return;
        }
        let Some(ready) = &self.ready else {
            return;
        };
        self.phase = match status.scoped_audition.as_ref() {
            Some(scoped) if scoped.id == ready.audition.id => match scoped.phase {
                ScopedAuditionPhase::Pending => ComparisonControllerPhase::Publishing,
                ScopedAuditionPhase::Active => ComparisonControllerPhase::Active,
            },
            _ => ComparisonControllerPhase::Ready,
        };
    }

    /// Invalidate only when the publication can affect this experiment. The
    /// current implementation widens that to the whole comparison product;
    /// future coverage/audio tiles can consume the same reasons per tile.
    pub fn observe_publication(
        &mut self,
        publication: &ProjectPublication,
    ) -> Option<ComparisonInvalidation> {
        self.observe_change_set(
            publication.generation,
            publication.revisions,
            publication.change_set.as_ref(),
        )
    }

    /// The publication adapter above is the normal entry point. This smaller
    /// seam lets headless controllers apply the same invalidation law without
    /// constructing or retaining a second project snapshot.
    pub fn observe_change_set(
        &mut self,
        publication_generation: u64,
        revisions: ProjectRevisions,
        change_set: Option<&ChangeSet>,
    ) -> Option<ComparisonInvalidation> {
        let selection = self.selection.as_ref()?;
        let mut reasons = Vec::new();
        let baseline_assets = self
            .producing_revision
            .map_or(selection.requested_at.assets, |revision| revision.assets);
        if revisions.assets != baseline_assets {
            reasons.push(ComparisonInvalidationReason::SourceAssets);
        }
        for (domain, generation) in &selection.dependencies.project {
            if revisions.domain(*domain) != *generation {
                reasons.push(ComparisonInvalidationReason::ExplanationDependency(*domain));
            }
        }
        if let Some(changes) = change_set {
            if changes.routing_changed {
                reasons.push(ComparisonInvalidationReason::Routing);
            }
            if change_set_overlaps(changes, selection.span) {
                reasons.push(ComparisonInvalidationReason::AudioOverlap);
            }
        }
        reasons.sort_unstable_by_key(invalidation_reason_order);
        reasons.dedup();
        if reasons.is_empty() {
            return None;
        }
        let invalidation = ComparisonInvalidation {
            publication_generation,
            revision: revisions.aggregate,
            reasons,
        };
        self.phase = ComparisonControllerPhase::Stale(invalidation.clone());
        Some(invalidation)
    }

    /// Pin the exact samples currently active in the shared renderer. A ready
    /// but not-yet-published selection cannot be mislabeled as audible export.
    pub fn pin_audible_export(
        &self,
        audio: &ProjectAudioController,
    ) -> Result<ComparisonExportPin, ComparisonControllerError> {
        self.pin_audible_export_from_status(&audio.status())
    }

    /// Status-only form for GPUI/session adapters that already receive the
    /// shared audio controller's event stream.
    pub fn pin_audible_export_from_status(
        &self,
        status: &ProjectAudioStatus,
    ) -> Result<ComparisonExportPin, ComparisonControllerError> {
        let ready = self
            .ready
            .as_ref()
            .ok_or(ComparisonControllerError::NoAudibleProduct)?;
        let active = status
            .scoped_audition
            .as_ref()
            .ok_or(ComparisonControllerError::SelectionNotAudible)?;
        if active.id != ready.audition.id || active.phase != ScopedAuditionPhase::Active {
            return Err(ComparisonControllerError::SelectionNotAudible);
        }
        Ok(ComparisonExportPin {
            selection_generation: ready.generation,
            audition: ready.audition.id,
            producing_revision: ready.product.produced_by.plan.revisions,
            product: Arc::clone(&ready.product),
        })
    }

    pub fn status(&self) -> ComparisonControllerStatus {
        ComparisonControllerStatus {
            owner: self.owner,
            selection: self.selection.clone(),
            phase: self.phase.clone(),
            producing_revision: self.producing_revision,
            pcm_digest: self.ready.as_ref().map(|ready| ready.product.id.pcm),
            audition: self.ready.as_ref().map(|ready| ready.audition.id),
        }
    }

    fn require_current(
        &self,
        request: &ComparisonSelectionRequest,
    ) -> Result<(), ComparisonControllerError> {
        let desired = self
            .selection
            .as_ref()
            .ok_or(ComparisonControllerError::NoSelection)?;
        if desired != request {
            return Err(ComparisonControllerError::ObsoleteCompletion {
                completed: request.generation,
                desired: desired.generation,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ComparisonExportPin {
    pub selection_generation: u64,
    pub audition: TimelineAuditionId,
    pub producing_revision: ProjectRevisionStamp,
    product: Arc<RenderProduct>,
}

impl ComparisonExportPin {
    pub fn render(&self) -> Result<RuntimeRenderedAudio, ComparisonControllerError> {
        let format = AudioFormat::new(
            self.product.id.format.sample_rate.get(),
            self.product.id.format.channels.get(),
        )
        .map_err(|error| ComparisonControllerError::Audio(error.to_string()))?;
        Ok(RuntimeRenderedAudio {
            plan: self.product.produced_by.plan.clone(),
            scope: self.product.produced_by.scope.clone(),
            origin_frame: self.product.produced_by.core.start,
            audio: ProjectAudio::new(format, self.product.shared_interleaved())
                .map_err(|error| ComparisonControllerError::Audio(error.to_string()))?,
            pcm_digest: self.product.id.pcm,
        })
    }
}

fn stale_dependencies(
    dependencies: &crate::explanation::ExplanationDependencyPin,
    revisions: ProjectRevisions,
) -> Option<ComparisonInvalidationReason> {
    dependencies
        .project
        .iter()
        .find_map(|(domain, generation)| {
            (revisions.domain(*domain) != *generation)
                .then_some(ComparisonInvalidationReason::ExplanationDependency(*domain))
        })
}

fn change_set_overlaps(changes: &ChangeSet, span: RenderSpan) -> bool {
    changes.audio.values().any(|impact| match impact {
        BusImpact::Whole => true,
        BusImpact::Ranges(ranges) => ranges
            .iter()
            .any(|range| range.start < span.end && span.start < range.end),
    })
}

fn invalidation_reason_order(reason: &ComparisonInvalidationReason) -> (u8, u8) {
    match reason {
        ComparisonInvalidationReason::SourceAssets => (0, 0),
        ComparisonInvalidationReason::ExplanationDependency(domain) => {
            (1, project_domain_order(*domain))
        }
        ComparisonInvalidationReason::Routing => (2, 0),
        ComparisonInvalidationReason::AudioOverlap => (3, 0),
    }
}

const fn project_domain_order(domain: ProjectDomain) -> u8 {
    match domain {
        ProjectDomain::Arrangement => 0,
        ProjectDomain::Sequencer => 1,
        ProjectDomain::Automation => 2,
        ProjectDomain::Assets => 3,
        ProjectDomain::Mixer => 4,
        ProjectDomain::SampleKits => 5,
        ProjectDomain::Air => 6,
        ProjectDomain::Bindings => 7,
    }
}

const fn stamped_domain(revisions: ProjectRevisionStamp, domain: ProjectDomain) -> u64 {
    match domain {
        ProjectDomain::Arrangement => revisions.arrangement,
        ProjectDomain::Sequencer => revisions.sequencer,
        ProjectDomain::Automation => revisions.automation,
        ProjectDomain::Assets => revisions.assets,
        ProjectDomain::Mixer => revisions.mixer,
        ProjectDomain::SampleKits => revisions.sample_kits,
        ProjectDomain::Air => revisions.air,
        ProjectDomain::Bindings => revisions.bindings,
    }
}

#[derive(Debug)]
pub enum ComparisonControllerError {
    ZeroOwner,
    GenerationExhausted,
    NoSelection,
    NoAudibleProduct,
    SelectionNotAudible,
    StaleSelection(u64),
    ObsoleteCompletion {
        completed: u64,
        desired: u64,
    },
    ObsoleteEffect {
        completed: u64,
        desired: u64,
    },
    ProductsDoNotMatchSelection,
    ObservationChanged,
    SourceAssetRevisionMismatch {
        requested: u64,
        product: u64,
    },
    RequestWasAlreadyStale(ComparisonInvalidationReason),
    MixedPlans,
    ProductFormatMismatch,
    ProductSpanMismatch {
        expected: RenderSpan,
        actual: RenderSpan,
    },
    UnexpectedScope {
        expected: RenderScope,
        actual: RenderScope,
    },
    UnexpectedPartition,
    PcmDigestMismatch {
        recorded: ExactDigest,
        actual: ExactDigest,
    },
    AlignedDigestMismatch {
        recorded: ExactRenderDigest,
        actual: ExactRenderDigest,
    },
    DependencyRevisionMismatch {
        domain: ProjectDomain,
        observation: u64,
        product: u64,
    },
    Definition(String),
    Products(String),
    Audio(String),
    ProjectAudio(ProjectAudioControllerError),
    Runtime(RenderRuntimeError),
}

impl fmt::Display for ComparisonControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "comparison controller: {self:?}")
    }
}

impl Error for ComparisonControllerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectAudio(error) => Some(error),
            Self::Runtime(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProjectAudioControllerError> for ComparisonControllerError {
    fn from(error: ProjectAudioControllerError) -> Self {
        Self::ProjectAudio(error)
    }
}

impl From<RenderRuntimeError> for ComparisonControllerError {
    fn from(error: RenderRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_catalog::sha256_content;
    use crate::aspect::{ChannelMask, FrameSpan};
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::comparison::ComparisonMetrics;
    use crate::explanation::{ExplanationDependencyPin, ExplanationId};
    use crate::ontology::{Producer, Provenance};
    use crate::render_plan::EngineRecipeStamp;
    use crate::render_runtime::project_revision_stamp;
    use crate::render_validation::GoldenFingerprint;

    fn revisions() -> ProjectRevisions {
        ProjectRevisions {
            aggregate: 11,
            arrangement: 2,
            sequencer: 3,
            automation: 4,
            assets: 5,
            mixer: 6,
            sample_kits: 7,
            air: 8,
            bindings: 9,
        }
    }

    fn definition() -> ComparisonDefinition {
        ComparisonDefinition {
            id: ComparisonId(41),
            label: "aligned null".into(),
            source: crate::comparison::SourceCitation {
                asset: AssetId(7),
                source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(4)).unwrap(),
                project_span: FrameSpan::new(100, 104).unwrap(),
                channels: ChannelMask(0b11),
            },
            explanation: ExplanationId(13),
            provenance: Provenance {
                producer: Producer::Human { name: None },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
        }
    }

    fn fingerprint() -> GoldenFingerprint {
        GoldenFingerprint {
            version: GoldenFingerprint::VERSION,
            sample_rate: 48_000,
            channels: 2,
            frames: 4,
            first_active_offset: Some(0),
            last_active_offset: Some(3),
            peak_millionths: 1,
            rms_millionths: 1,
            dc_millionths: 0,
            block_energy_hash: 1,
        }
    }

    fn plan() -> RenderPlanId {
        let format = RenderFormat::new(48_000, 2).unwrap();
        RenderPlanId::new(
            99,
            ExactDigest::new([7; 32]),
            project_revision_stamp(revisions()),
            RenderSpan::new(0, 1_000).unwrap(),
            EngineRecipeStamp::new(1, format, 64, 0, ExactDigest::new([9; 32])).unwrap(),
            Vec::new(),
        )
        .unwrap()
    }

    fn product(
        plan: &RenderPlanId,
        scope: RenderScope,
        samples: &[f32],
    ) -> (Arc<RenderProduct>, ExactRenderDigest) {
        let span = RenderSpan::new(100, 104).unwrap();
        let key = crate::render_products::RenderProductKey::new(
            plan.clone(),
            scope,
            span,
            ProductPartition::ContiguousRun {
                anchor_frame: 100,
                sequence: 0,
            },
            ExactDigest::new([3; 32]),
        )
        .unwrap();
        let pcm: Arc<[f32]> = Arc::from(samples);
        let product = Arc::new(
            RenderProduct::new(canonical_pcm_digest(&pcm), key, Arc::clone(&pcm)).unwrap(),
        );
        let audio = ProjectAudio::new(AudioFormat::new(48_000, 2).unwrap(), pcm).unwrap();
        let aligned = exact_audio_digest(&audio, 100).unwrap();
        (product, aligned)
    }

    fn fixture() -> (
        ComparisonDefinition,
        ComparisonObservation,
        Arc<ComparisonAuditionProducts>,
    ) {
        let definition = definition();
        let plan = plan();
        let (source, source_digest) = product(
            &plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_SOURCE_SCOPE_NAMESPACE,
                local: definition.id.0,
            }),
            &[1.0, 0.5, 0.8, 0.4, 0.6, 0.3, 0.4, 0.2],
        );
        let (construction, construction_digest) = product(
            &plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_CONSTRUCTION_SCOPE_NAMESPACE,
                local: definition.explanation.0,
            }),
            &[0.8, 0.4, 0.6, 0.3, 0.4, 0.2, 0.2, 0.1],
        );
        let (residual, residual_digest) = product(
            &plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_RESIDUAL_SCOPE_NAMESPACE,
                local: definition.id.0,
            }),
            &[0.2, 0.1, 0.2, 0.1, 0.2, 0.1, 0.2, 0.1],
        );
        let observation = ComparisonObservation {
            dependencies: ExplanationDependencyPin::from_dependencies(
                revisions(),
                [ProjectDomain::Arrangement, ProjectDomain::Mixer],
                [],
            ),
            source_digest,
            construction_digest,
            residual_digest,
            construction_fingerprint: fingerprint(),
            residual_fingerprint: fingerprint(),
            metrics: ComparisonMetrics::default(),
        };
        let products = ComparisonRenderProducts {
            source,
            construction,
            residual,
        };
        let validated =
            ComparisonAuditionProducts::validate(&definition, observation.clone(), products)
                .unwrap();
        (definition, observation, Arc::new(validated))
    }

    fn published_audition(effect: ComparisonAudioEffect) -> Arc<TimelineAudition> {
        match effect {
            ComparisonAudioEffect::Publish { audition, .. } => audition,
            ComparisonAudioEffect::Clear { .. } => panic!("expected audible comparison channel"),
        }
    }

    #[test]
    fn switching_channels_preserves_exact_alignment_and_replaces_one_owner() {
        let (definition, observation, products) = fixture();
        let mut controller = ComparisonController::new(5).unwrap();
        let source_request = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Source,
            )
            .unwrap();
        let source = published_audition(
            controller
                .accept_products(&source_request, Arc::clone(&products))
                .unwrap(),
        );
        let construction_request = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Construction,
            )
            .unwrap();
        let construction = published_audition(
            controller
                .accept_products(&construction_request, Arc::clone(&products))
                .unwrap(),
        );
        let residual_request = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Residual,
            )
            .unwrap();
        let residual = published_audition(
            controller
                .accept_products(&residual_request, products)
                .unwrap(),
        );

        assert_eq!(source.id.owner, construction.id.owner);
        assert_eq!(construction.id.owner, residual.id.owner);
        assert_eq!(source.span, construction.span);
        assert_eq!(construction.span, residual.span);
        assert_eq!(source.format, construction.format);
        assert_eq!(construction.format, residual.format);
        assert_eq!(source.subject, AuditionSubject::Source);
        assert_eq!(construction.subject, AuditionSubject::Construction);
        assert_eq!(residual.subject, AuditionSubject::Residual);
        assert!(source.id.revision < construction.id.revision);
        assert!(construction.id.revision < residual.id.revision);
        assert_ne!(source.id.content, construction.id.content);
        assert_ne!(construction.id.content, residual.id.content);
    }

    #[test]
    fn late_product_cannot_replace_newer_channel_selection() {
        let (definition, observation, products) = fixture();
        let mut controller = ComparisonController::new(5).unwrap();
        let old = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Source,
            )
            .unwrap();
        let current = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Residual,
            )
            .unwrap();
        assert!(matches!(
            controller.accept_products(&old, Arc::clone(&products)),
            Err(ComparisonControllerError::ObsoleteCompletion { .. })
        ));
        let audition = published_audition(controller.accept_products(&current, products).unwrap());
        assert_eq!(audition.subject, AuditionSubject::Residual);
        assert_eq!(controller.status().audition, Some(audition.id));
    }

    #[test]
    fn excess_is_selectable_but_never_misrepresented_as_pcm() {
        let (definition, observation, products) = fixture();
        let mut controller = ComparisonController::new(5).unwrap();
        let request = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Excess,
            )
            .unwrap();
        let effect = controller.accept_products(&request, products).unwrap();
        assert!(matches!(effect, ComparisonAudioEffect::Clear { .. }));
        assert_eq!(
            controller.status().phase,
            ComparisonControllerPhase::CoverageOnly
        );
        assert_eq!(controller.status().pcm_digest, None);
    }

    #[test]
    fn export_pin_is_the_exact_active_audition_product() {
        let (definition, observation, products) = fixture();
        let mut controller = ComparisonController::new(5).unwrap();
        let request = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Residual,
            )
            .unwrap();
        let audition = published_audition(controller.accept_products(&request, products).unwrap());
        let mut status = ProjectAudioStatus::default();
        status.scoped_audition = Some(crate::project_session::ScopedAuditionStatus {
            id: audition.id,
            owner: audition.id.owner,
            subject: audition.subject,
            mix: audition.mix,
            span: audition.span,
            phase: ScopedAuditionPhase::Active,
        });
        let pin = controller.pin_audible_export_from_status(&status).unwrap();
        let exported = pin.render().unwrap();
        assert_eq!(pin.audition, audition.id);
        assert_eq!(exported.origin_frame, audition.span.start);
        assert_eq!(exported.pcm_digest, audition.id.content);
        assert_eq!(exported.audio.interleaved(), audition.interleaved());

        status.scoped_audition.as_mut().unwrap().phase = ScopedAuditionPhase::Pending;
        assert!(matches!(
            controller.pin_audible_export_from_status(&status),
            Err(ComparisonControllerError::SelectionNotAudible)
        ));
    }

    #[test]
    fn change_set_invalidation_is_overlap_and_dependency_aware() {
        let (definition, observation, products) = fixture();
        let mut controller = ComparisonController::new(5).unwrap();
        let request = controller
            .select(
                &definition,
                &observation,
                revisions(),
                ComparisonChannel::Source,
            )
            .unwrap();
        controller.accept_products(&request, products).unwrap();

        let bus = crate::mixer::BusId::from_raw(1);
        let mut outside = ChangeSet::default();
        outside.invalidate_range(bus, crate::change_set::AudioRange::new(200, 220).unwrap());
        let mut next = revisions();
        next.aggregate += 1;
        assert_eq!(controller.observe_change_set(2, next, Some(&outside)), None);

        let mut overlap = ChangeSet::default();
        overlap.invalidate_range(bus, crate::change_set::AudioRange::new(103, 120).unwrap());
        let invalidation = controller
            .observe_change_set(3, next, Some(&overlap))
            .unwrap();
        assert_eq!(
            invalidation.reasons,
            vec![ComparisonInvalidationReason::AudioOverlap]
        );
    }

    #[test]
    fn canonical_product_digest_is_checked_before_audition() {
        let (definition, observation, products) = fixture();
        let bad_key = products.source.produced_by.clone();
        let bad_source = Arc::new(
            RenderProduct::new(
                ExactDigest::new([0x55; 32]),
                bad_key,
                products.source.shared_interleaved(),
            )
            .unwrap(),
        );
        let result = ComparisonAuditionProducts::validate(
            &definition,
            observation,
            ComparisonRenderProducts {
                source: bad_source,
                construction: Arc::clone(&products.construction),
                residual: Arc::clone(&products.residual),
            },
        );
        assert!(matches!(
            result,
            Err(ComparisonControllerError::PcmDigestMismatch { .. })
        ));
    }

    #[test]
    fn digest_pins_are_strong_and_position_sensitive() {
        let audio = ProjectAudio::from_interleaved(
            AudioFormat::new(48_000, 2).unwrap(),
            vec![0.25, -0.25, 0.5, -0.5],
        )
        .unwrap();
        let at_zero = exact_audio_digest(&audio, 0).unwrap();
        let shifted = exact_audio_digest(&audio, 1).unwrap();
        assert_ne!(at_zero, shifted);
        assert_eq!(sha256_content(b"test", &[b"same"]).is_strong(), true);
    }
}
