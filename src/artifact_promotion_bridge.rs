//! Artifact-backed deprojection promotion followed by an exact comparison.
//!
//! This is an orchestration boundary, not another renderer. It validates one
//! content-addressed analysis payload and its candidate evidence, compiles one
//! atomic ordinary-project promotion, hands the resulting publication to the
//! existing project-audio render path, and finally captures comparison work
//! through `ComparisonProductExecutor`.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::comparison_hydration::{
    ArtifactComparisonPayload, ArtifactComparisonPin, ArtifactComparisonSignal,
};
use crate::artifact_catalog::{
    ArtifactCatalog, ArtifactCatalogError, ArtifactDescriptor, ArtifactId, DigestAlgorithm,
};
use crate::aspect::{Aspect, FrameSpan};
use crate::assets::{AssetFrameRange, SampleFrames};
use crate::comparison::{
    ComparisonDefinition, ComparisonId, ComparisonObservation, SourceCitation,
};
use crate::comparison_controller::{
    ComparisonChannel, ComparisonController, ComparisonControllerError, ComparisonSelectionRequest,
};
use crate::comparison_runtime::executor::{
    ComparisonProductExecutor, ComparisonProductExecutorError, ComparisonProductJob,
    ComparisonProductRecipe, ComparisonSemanticSnapshot,
};
use crate::comparison_runtime::{
    ComparisonRuntime, ComparisonRuntimeError, PcmComparisonSourceResolver,
};
use crate::daw_project::ProjectRevisions;
use crate::daw_render::RenderCancellation;
use crate::deprojection_execution::promotion::{
    compile_promotion, execute_promotion, CreatedObject, PromotionBindings, PromotionCommandPlan,
    PromotionCompileError, PromotionExecutionError, PromotionPlacement, PromotionRequest,
    PromotionResult,
};
use crate::deprojection_program::{DeprojectionCandidate, EvidenceRef, SourceClaim, SourceProgram};
use crate::explanation::{
    ExplanationDefinition, ExplanationEvidenceRef, ExplanationId, ExplanationScope,
};
use crate::explanation_adapters::{
    CatalogAnalysisResolver, ExclusiveScheduleIsolationBackend, ResolvingExplanationCompiler,
    ScheduleDawScopeResolver,
};
use crate::interpretation::{InterpretationCommand, InterpretationError, InterpretationStore};
use crate::ontology::Provenance;
use crate::project_audio_controller::{
    ProjectAudioController, ProjectAudioControllerError, ProjectAudioRenderJob,
    ProjectAudioRenderRecipe,
};
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::render_runtime::AuditionOwner;

#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactPromotionComparisonTarget {
    pub comparison: ComparisonId,
    pub explanation: ExplanationId,
    pub label: String,
    pub source: SourceCitation,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactPromotionComparisonRequest {
    pub artifact: ArtifactId,
    pub artifact_pin: ArtifactComparisonPin,
    pub expected_document_generation: u64,
    pub candidate: DeprojectionCandidate,
    pub bindings: PromotionBindings,
    pub placement: PromotionPlacement,
    pub target: ArtifactPromotionComparisonTarget,
    pub recipe: ComparisonProductRecipe,
}

/// Pure, revision-pinned work. Creating a plan never mutates the project.
#[derive(Clone, Debug)]
pub struct ArtifactPromotionComparisonPlan {
    descriptor: ArtifactDescriptor,
    payload: Arc<ArtifactComparisonPayload>,
    candidate: DeprojectionCandidate,
    document_generation: u64,
    base_publication_generation: u64,
    base_revisions: ProjectRevisions,
    promotion: PromotionCommandPlan,
    target: ArtifactPromotionComparisonTarget,
    recipe: ComparisonProductRecipe,
}

impl ArtifactPromotionComparisonPlan {
    pub fn descriptor(&self) -> &ArtifactDescriptor {
        &self.descriptor
    }

    pub fn payload(&self) -> &Arc<ArtifactComparisonPayload> {
        &self.payload
    }

    pub const fn base_revisions(&self) -> ProjectRevisions {
        self.base_revisions
    }

    pub const fn base_publication_generation(&self) -> u64 {
        self.base_publication_generation
    }

    /// Commit exactly one aggregate command envelope. Cancellation is checked
    /// before the atomic session mutation; once the envelope commits, the
    /// returned result always reports that committed edit.
    pub fn execute(
        self,
        session: &mut ProjectSession,
        cancellation: &RenderCancellation,
    ) -> Result<ArtifactPromotionComparisonResult, ArtifactPromotionBridgeError> {
        if cancellation.is_cancelled() {
            return Err(ArtifactPromotionBridgeError::Cancelled);
        }
        require_session_pin(
            session,
            self.document_generation,
            self.base_publication_generation,
            self.base_revisions,
        )?;
        let artifact_pin = self.payload.pin;
        let promotion = execute_promotion(session, self.promotion)?;
        if promotion.project.publication.generation <= self.base_publication_generation {
            return Err(ArtifactPromotionBridgeError::PublicationDidNotAdvance {
                before: self.base_publication_generation,
                after: promotion.project.publication.generation,
            });
        }
        Ok(ArtifactPromotionComparisonResult {
            descriptor: self.descriptor,
            payload: self.payload,
            artifact_pin,
            candidate: self.candidate,
            document_generation: self.document_generation,
            promotion,
            target: self.target,
            recipe: self.recipe,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactPromotionComparisonResult {
    pub descriptor: ArtifactDescriptor,
    pub payload: Arc<ArtifactComparisonPayload>,
    pub artifact_pin: ArtifactComparisonPin,
    /// Full immutable analytic provenance, including origin, source claims,
    /// structural score, and caveats. Term-level evidence/derivation is also
    /// retained in `promotion.provenance`.
    pub candidate: DeprojectionCandidate,
    pub document_generation: u64,
    pub promotion: PromotionResult,
    pub target: ArtifactPromotionComparisonTarget,
    pub recipe: ComparisonProductRecipe,
}

impl ArtifactPromotionComparisonResult {
    pub fn promoted_revisions(&self) -> ProjectRevisions {
        self.promotion.project.publication.revisions
    }

    pub fn promoted_publication_generation(&self) -> u64 {
        self.promotion.project.publication.generation
    }

    /// Feed the committed publication to the authoritative project renderer.
    /// The returned job carries the controller's cancellation token.
    pub fn request_shared_render(
        &self,
        session: &ProjectSession,
        audio: &mut ProjectAudioController,
        recipe: ProjectAudioRenderRecipe,
        cancellation: &RenderCancellation,
    ) -> Result<ProjectAudioRenderJob, ArtifactPromotionBridgeError> {
        if cancellation.is_cancelled() {
            return Err(ArtifactPromotionBridgeError::Cancelled);
        }
        self.require_promoted_head(session)?;
        if !recipe.extent.contains_span(self.target_span()?) {
            return Err(ArtifactPromotionBridgeError::RenderRecipeOutsideComparison);
        }
        Ok(audio.request_render(self.promotion.project.publication.clone(), recipe))
    }

    /// Build the exact updated observation from the currently active shared
    /// schedule, then delegate the captured job to the existing executor.
    /// This preflight is necessary because executor selection is digest-pinned
    /// and therefore requires the updated observation before capture.
    pub fn capture_updated_comparison(
        &self,
        session: &ProjectSession,
        audio: &ProjectAudioController,
        controller: &mut ComparisonController,
        executor: &mut ComparisonProductExecutor,
        channel: ComparisonChannel,
        cancellation: &RenderCancellation,
    ) -> Result<ArtifactPromotionComparisonCapture, ArtifactPromotionBridgeError> {
        if cancellation.is_cancelled() {
            return Err(ArtifactPromotionBridgeError::Cancelled);
        }
        self.require_promoted_head(session)?;
        let snapshot = session.project_snapshot()?.clone();
        let revisions = snapshot.revisions();
        let executable = {
            let cohort = audio.runtime().service().active_cohort().ok_or(
                ArtifactPromotionBridgeError::SharedRenderNotReady {
                    promoted: revisions,
                    active: None,
                },
            )?;
            audio.runtime().executable_plan(&cohort.id.plan)?
        };
        let active = executable.schedule.project_revision();
        if active != revisions {
            return Err(ArtifactPromotionBridgeError::SharedRenderNotReady {
                promoted: revisions,
                active: Some(active),
            });
        }
        let scope = promoted_scope(&self.promotion.created)?;
        require_exclusive_schedule(&scope, executable.schedule.render_schedule())?;
        let mut interpretations = InterpretationStore::new();
        let explanation = ExplanationDefinition {
            id: self.target.explanation,
            label: self.target.label.clone(),
            scope,
            extent: Aspect::Time(self.target.source.project_span),
            evidence: vec![ExplanationEvidenceRef::Artifact(self.descriptor.id)],
            provenance: self.target.provenance.clone(),
        };
        let definition = ComparisonDefinition {
            id: self.target.comparison,
            label: self.target.label.clone(),
            source: self.target.source,
            explanation: self.target.explanation,
            provenance: self.target.provenance.clone(),
        };
        interpretations.apply(&[
            InterpretationCommand::PutExplanation {
                before: None,
                after: Some(explanation),
            },
            InterpretationCommand::PutComparison {
                before: None,
                after: Some(definition.clone()),
            },
        ])?;

        let artifacts = Arc::new(ArtifactCatalog::new());
        let daw = ScheduleDawScopeResolver::new(
            &snapshot.project,
            Arc::clone(&executable.schedule),
            Arc::new(ExclusiveScheduleIsolationBackend),
        )?;
        let analysis = CatalogAnalysisResolver {
            catalog: &artifacts,
        };
        let compiler = ResolvingExplanationCompiler {
            revisions,
            definitions: &interpretations,
            daw: &daw,
            analysis: &analysis,
        };
        let source = PcmComparisonSourceResolver {
            assets: &snapshot.project.state().domains.assets,
            pcm: &snapshot.pcm,
        };
        let execution = ComparisonRuntime {
            interpretations: &interpretations,
            explanations: &compiler,
            sources: &source,
        }
        .execute(&definition, self.recipe.coverage, cancellation)?;
        let observation = execution.observation;
        interpretations.apply(&[InterpretationCommand::PutObservation {
            comparison: definition.id,
            before: None,
            after: Some(observation.clone()),
        }])?;
        let semantics = ComparisonSemanticSnapshot {
            interpretations: Arc::new(interpretations),
            artifacts,
        };
        let request = controller.select(&definition, &observation, revisions, channel)?;
        let job = executor.capture(
            controller.owner(),
            request.clone(),
            session,
            audio,
            semantics,
            self.recipe,
        )?;
        Ok(ArtifactPromotionComparisonCapture {
            owner: controller.owner(),
            definition,
            observation,
            request,
            job,
        })
    }

    pub fn undo(
        &self,
        session: &mut ProjectSession,
    ) -> Result<crate::project_session::ProjectEditReceipt, ArtifactPromotionBridgeError> {
        self.require_promoted_head(session)?;
        Ok(self.promotion.undo(session)?)
    }

    fn require_promoted_head(
        &self,
        session: &ProjectSession,
    ) -> Result<(), ArtifactPromotionBridgeError> {
        require_session_pin(
            session,
            self.document_generation,
            self.promoted_publication_generation(),
            self.promoted_revisions(),
        )
    }

    fn target_span(&self) -> Result<crate::render_plan::RenderSpan, ArtifactPromotionBridgeError> {
        crate::render_plan::RenderSpan::new(
            self.target.source.project_span.start,
            self.target.source.project_span.end,
        )
        .map_err(|error| ArtifactPromotionBridgeError::InvalidTarget(error.to_string()))
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactPromotionComparisonCapture {
    pub owner: AuditionOwner,
    pub definition: ComparisonDefinition,
    pub observation: ComparisonObservation,
    pub request: ComparisonSelectionRequest,
    pub job: ComparisonProductJob,
}

pub fn plan_artifact_promotion_comparison(
    session: &ProjectSession,
    catalog: &ArtifactCatalog,
    request: ArtifactPromotionComparisonRequest,
    cancellation: &RenderCancellation,
) -> Result<ArtifactPromotionComparisonPlan, ArtifactPromotionBridgeError> {
    if cancellation.is_cancelled() {
        return Err(ArtifactPromotionBridgeError::Cancelled);
    }
    let snapshot = session.project_snapshot()?.clone();
    let revisions = snapshot.revisions();
    require_session_pin(
        session,
        request.expected_document_generation,
        request.artifact_pin.publication_generation,
        request.artifact_pin.project_revisions,
    )?;
    if request.artifact_pin.project_revisions != revisions {
        return Err(ArtifactPromotionBridgeError::StaleArtifactRevision {
            pinned: request.artifact_pin.project_revisions,
            current: revisions,
        });
    }
    let descriptor = catalog.descriptor(request.artifact).cloned().ok_or(
        ArtifactPromotionBridgeError::MissingArtifact(request.artifact),
    )?;
    let payload = catalog
        .get::<ArtifactComparisonPayload>(request.artifact)
        .map_err(|error| match error {
            ArtifactCatalogError::Missing(id) => ArtifactPromotionBridgeError::MissingArtifact(id),
            _ => ArtifactPromotionBridgeError::PayloadTypeMismatch(request.artifact),
        })?;
    validate_payload(&descriptor, &payload, request.artifact_pin, cancellation)?;
    validate_candidate(&descriptor, &request.candidate)?;
    validate_target(
        &descriptor,
        &request.candidate.program,
        request.placement,
        &request.target,
    )?;
    if cancellation.is_cancelled() {
        return Err(ArtifactPromotionBridgeError::Cancelled);
    }
    let candidate = request.candidate;
    let promotion = compile_promotion(
        &snapshot,
        PromotionRequest {
            candidate: candidate.id,
            expected_project_revision: revisions.aggregate,
            program: candidate.program.clone(),
            bindings: request.bindings,
            placement: request.placement,
        },
    )?;
    Ok(ArtifactPromotionComparisonPlan {
        descriptor,
        payload,
        candidate,
        document_generation: request.expected_document_generation,
        base_publication_generation: request.artifact_pin.publication_generation,
        base_revisions: revisions,
        promotion,
        target: request.target,
        recipe: request.recipe,
    })
}

fn validate_payload(
    descriptor: &ArtifactDescriptor,
    payload: &ArtifactComparisonPayload,
    expected: ArtifactComparisonPin,
    cancellation: &RenderCancellation,
) -> Result<(), ArtifactPromotionBridgeError> {
    if payload.pin != expected {
        return Err(ArtifactPromotionBridgeError::ArtifactPinMismatch {
            expected,
            actual: payload.pin,
        });
    }
    if expected.artifact != descriptor.id
        || expected.source_digest != descriptor.source_digest
        || expected.recipe_digest != descriptor.recipe_digest
    {
        return Err(ArtifactPromotionBridgeError::DescriptorPinMismatch(
            descriptor.id,
        ));
    }
    let mut rebuilt = Vec::with_capacity(payload.signals().len());
    for (_, signal) in payload.signals() {
        if cancellation.is_cancelled() {
            return Err(ArtifactPromotionBridgeError::Cancelled);
        }
        let exact =
            ArtifactComparisonSignal::new(signal.key, signal.origin_frame, signal.audio.clone())?;
        if exact.digest != signal.digest {
            return Err(ArtifactPromotionBridgeError::SignalDigestMismatch {
                artifact: descriptor.id,
                key: signal.key,
            });
        }
        rebuilt.push(exact);
    }
    ArtifactComparisonPayload::new(descriptor, expected, rebuilt, cancellation)?;
    Ok(())
}

fn validate_candidate(
    descriptor: &ArtifactDescriptor,
    candidate: &DeprojectionCandidate,
) -> Result<(), ArtifactPromotionBridgeError> {
    let mut linked = false;
    for claim in &candidate.source_claims {
        if let Some(artifact) = claim.artifact {
            if artifact != descriptor.id {
                return Err(ArtifactPromotionBridgeError::UnpinnedEvidenceArtifact(
                    artifact,
                ));
            }
            validate_claim(descriptor, claim)?;
            linked = true;
        }
    }
    for term in candidate.program.terms.values() {
        let term_artifacts = term
            .evidence
            .iter()
            .chain(&term.derivation.premises)
            .filter_map(|evidence| match evidence {
                EvidenceRef::Artifact(artifact) => Some(*artifact),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        for artifact in term_artifacts {
            if artifact != descriptor.id {
                return Err(ArtifactPromotionBridgeError::UnpinnedEvidenceArtifact(
                    artifact,
                ));
            }
            if term.derivation.recipe != descriptor.recipe_digest {
                return Err(ArtifactPromotionBridgeError::EvidenceRecipeMismatch {
                    artifact,
                    term: term.id,
                });
            }
            linked = true;
        }
    }
    if !linked {
        return Err(ArtifactPromotionBridgeError::CandidateMissingArtifactEvidence(descriptor.id));
    }
    Ok(())
}

fn validate_claim(
    descriptor: &ArtifactDescriptor,
    claim: &SourceClaim,
) -> Result<(), ArtifactPromotionBridgeError> {
    if claim.output_digest != descriptor.output_digest {
        return Err(ArtifactPromotionBridgeError::ClaimOutputMismatch(
            descriptor.id,
        ));
    }
    if claim.producer_recipe != descriptor.recipe_digest {
        return Err(ArtifactPromotionBridgeError::ClaimRecipeMismatch(
            descriptor.id,
        ));
    }
    validate_material(descriptor, &claim.source)
}

fn validate_target(
    descriptor: &ArtifactDescriptor,
    program: &SourceProgram,
    placement: PromotionPlacement,
    target: &ArtifactPromotionComparisonTarget,
) -> Result<(), ArtifactPromotionBridgeError> {
    if target.comparison.0 == 0 || target.explanation.0 == 0 || target.label.trim().is_empty() {
        return Err(ArtifactPromotionBridgeError::InvalidTarget(
            "comparison/explanation identities and label must be non-empty".into(),
        ));
    }
    validate_material(descriptor, &program.source)?;
    let expected_source_end = program
        .source
        .start_frame
        .checked_add(program.source.frame_count)
        .ok_or(ArtifactPromotionBridgeError::MaterialTooLarge)?;
    if target.source.source_range
        != AssetFrameRange::new(
            SampleFrames(program.source.start_frame),
            SampleFrames(expected_source_end),
        )
        .map_err(|error| ArtifactPromotionBridgeError::InvalidTarget(error.to_string()))?
    {
        return Err(ArtifactPromotionBridgeError::SourceRangeMismatch);
    }
    if target.source.project_span != descriptor.extent
        || placement.start_frame != descriptor.extent.start
    {
        return Err(ArtifactPromotionBridgeError::PlacementMismatch {
            descriptor: descriptor.extent,
            source: target.source.project_span,
            placement: placement.start_frame,
        });
    }
    target
        .source
        .validate()
        .map_err(|error| ArtifactPromotionBridgeError::InvalidTarget(error.to_string()))
}

fn validate_material(
    descriptor: &ArtifactDescriptor,
    material: &crate::deprojection_program::MaterialSpan,
) -> Result<(), ArtifactPromotionBridgeError> {
    material
        .validate()
        .map_err(|error| ArtifactPromotionBridgeError::InvalidCandidate(error.to_string()))?;
    let frames =
        u64::try_from(i128::from(descriptor.extent.end) - i128::from(descriptor.extent.start))
            .map_err(|_| ArtifactPromotionBridgeError::MaterialTooLarge)?;
    if frames != material.frame_count
        || descriptor.sample_rate != material.sample_rate_hz
        || descriptor.channels != material.channels
    {
        return Err(ArtifactPromotionBridgeError::MaterialGeometryMismatch);
    }
    if descriptor.source_digest.algorithm != DigestAlgorithm::Sha256
        || hex_digest(descriptor.source_digest.bytes) != material.material_sha256
    {
        return Err(ArtifactPromotionBridgeError::MaterialDigestMismatch);
    }
    Ok(())
}

fn promoted_scope(
    created: &[CreatedObject],
) -> Result<ExplanationScope, ArtifactPromotionBridgeError> {
    let mut scopes = created
        .iter()
        .filter_map(|created| match created {
            CreatedObject::AudioClip(clip) | CreatedObject::ExactAudioFallbackClip(clip) => {
                Some(ExplanationScope::ArrangementClip(*clip))
            }
            CreatedObject::SequencerPatternClip(clip) => Some(ExplanationScope::PatternClip(*clip)),
            _ => None,
        })
        .collect::<Vec<_>>();
    scopes.dedup();
    match scopes.len() {
        0 => Err(ArtifactPromotionBridgeError::NoPromotedAudibleScope),
        1 => Ok(scopes.remove(0)),
        count => Err(ArtifactPromotionBridgeError::MultiplePromotedAudibleScopes(
            count,
        )),
    }
}

/// Mirror the public proof required by `ExclusiveScheduleIsolationBackend` so
/// this bridge can return a typed integration refusal. Supporting a promoted
/// scope alongside other audible inputs requires the executor's smallest
/// future extension: accept an `Arc<dyn DawIsolationBackend>` at capture.
fn require_exclusive_schedule(
    scope: &ExplanationScope,
    schedule: &crate::daw_render::RenderSchedule,
) -> Result<(), ArtifactPromotionBridgeError> {
    let isolated = match scope {
        ExplanationScope::ArrangementClip(target) => {
            let clips = schedule.audio_clips();
            !clips.is_empty()
                && clips.iter().all(|clip| clip.id == *target)
                && schedule.blocks().iter().all(|block| {
                    block.sequencer_events.iter().all(|event| {
                        matches!(event.kind, crate::sequencer::ScheduledKind::LoopBoundary)
                    })
                })
        }
        ExplanationScope::PatternClip(target) => {
            schedule.audio_clips().is_empty()
                && schedule.blocks().iter().all(|block| {
                    block.sequencer_events.iter().all(|event| {
                        scheduled_pattern_clip(&event.kind).is_none_or(|clip| clip == *target)
                    })
                })
                && schedule.blocks().iter().any(|block| {
                    block
                        .sequencer_events
                        .iter()
                        .any(|event| scheduled_pattern_clip(&event.kind) == Some(*target))
                })
        }
        _ => false,
    };
    if isolated {
        Ok(())
    } else {
        Err(ArtifactPromotionBridgeError::IsolationBackendRequired(
            scope.clone(),
        ))
    }
}

fn scheduled_pattern_clip(
    kind: &crate::sequencer::ScheduledKind,
) -> Option<crate::sequencer::PatternClipId> {
    match kind {
        crate::sequencer::ScheduledKind::LoopBoundary => None,
        crate::sequencer::ScheduledKind::NoteOff { clip, .. }
        | crate::sequencer::ScheduledKind::NoteOn { clip, .. }
        | crate::sequencer::ScheduledKind::NoteExpression { clip, .. }
        | crate::sequencer::ScheduledKind::Trigger { clip, .. } => Some(*clip),
    }
}

fn require_session_pin(
    session: &ProjectSession,
    document_generation: u64,
    publication_generation: u64,
    revisions: ProjectRevisions,
) -> Result<(), ArtifactPromotionBridgeError> {
    if session.document_generation() != document_generation {
        return Err(ArtifactPromotionBridgeError::DocumentSuperseded {
            pinned: document_generation,
            current: session.document_generation(),
        });
    }
    if session.snapshot().generation != publication_generation {
        return Err(ArtifactPromotionBridgeError::PublicationSuperseded {
            pinned: publication_generation,
            current: session.snapshot().generation,
        });
    }
    let current = session.project_snapshot()?.revisions();
    if current != revisions {
        return Err(ArtifactPromotionBridgeError::StaleArtifactRevision {
            pinned: revisions,
            current,
        });
    }
    Ok(())
}

fn hex_digest(bytes: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[derive(Debug)]
pub enum ArtifactPromotionBridgeError {
    Cancelled,
    MissingArtifact(ArtifactId),
    PayloadTypeMismatch(ArtifactId),
    ArtifactPinMismatch {
        expected: ArtifactComparisonPin,
        actual: ArtifactComparisonPin,
    },
    DescriptorPinMismatch(ArtifactId),
    SignalDigestMismatch {
        artifact: ArtifactId,
        key: crate::explanation_adapters::ArtifactScopeKey,
    },
    CandidateMissingArtifactEvidence(ArtifactId),
    UnpinnedEvidenceArtifact(ArtifactId),
    EvidenceRecipeMismatch {
        artifact: ArtifactId,
        term: crate::deprojection_program::EditableTermId,
    },
    ClaimOutputMismatch(ArtifactId),
    ClaimRecipeMismatch(ArtifactId),
    MaterialDigestMismatch,
    MaterialGeometryMismatch,
    MaterialTooLarge,
    SourceRangeMismatch,
    PlacementMismatch {
        descriptor: FrameSpan,
        source: FrameSpan,
        placement: i64,
    },
    InvalidCandidate(String),
    InvalidTarget(String),
    DocumentSuperseded {
        pinned: u64,
        current: u64,
    },
    PublicationSuperseded {
        pinned: u64,
        current: u64,
    },
    StaleArtifactRevision {
        pinned: ProjectRevisions,
        current: ProjectRevisions,
    },
    PublicationDidNotAdvance {
        before: u64,
        after: u64,
    },
    RenderRecipeOutsideComparison,
    SharedRenderNotReady {
        promoted: ProjectRevisions,
        active: Option<ProjectRevisions>,
    },
    NoPromotedAudibleScope,
    MultiplePromotedAudibleScopes(usize),
    /// The shared schedule contains other audible inputs. The smallest honest
    /// extension is dependency-injecting `DawIsolationBackend` into executor
    /// capture; this bridge will not compile or bounce a second schedule.
    IsolationBackendRequired(ExplanationScope),
    Hydration(crate::artifact_catalog::comparison_hydration::ArtifactComparisonHydrationError),
    PromotionCompile(PromotionCompileError),
    PromotionExecution(PromotionExecutionError),
    Session(ProjectSessionError),
    ProjectAudio(ProjectAudioControllerError),
    Comparison(ComparisonRuntimeError),
    ComparisonController(ComparisonControllerError),
    ComparisonExecutor(ComparisonProductExecutorError),
    Explanation(crate::explanation::ExplanationError),
    Interpretation(InterpretationError),
    RenderRuntime(crate::render_runtime::RenderRuntimeError),
}

impl fmt::Display for ArtifactPromotionBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "artifact promotion/comparison bridge: {self:?}")
    }
}

impl Error for ArtifactPromotionBridgeError {}

macro_rules! bridge_from {
    ($source:ty, $variant:ident) => {
        impl From<$source> for ArtifactPromotionBridgeError {
            fn from(error: $source) -> Self {
                Self::$variant(error)
            }
        }
    };
}

bridge_from!(
    crate::artifact_catalog::comparison_hydration::ArtifactComparisonHydrationError,
    Hydration
);
bridge_from!(PromotionCompileError, PromotionCompile);
bridge_from!(PromotionExecutionError, PromotionExecution);
bridge_from!(ProjectSessionError, Session);
bridge_from!(ProjectAudioControllerError, ProjectAudio);
bridge_from!(ComparisonRuntimeError, Comparison);
bridge_from!(ComparisonControllerError, ComparisonController);
bridge_from!(ComparisonProductExecutorError, ComparisonExecutor);
bridge_from!(crate::explanation::ExplanationError, Explanation);
bridge_from!(InterpretationError, Interpretation);
bridge_from!(crate::render_runtime::RenderRuntimeError, RenderRuntime);

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::arrangement::ArrangementOperation;
    use crate::artifact_catalog::comparison_hydration::insert_artifact_comparison_payload;
    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::aspect::ChannelMask;
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::audio::{AudioFormat, ProjectAudio};
    use crate::command::{claims_for_commands, CommandEnvelope, DomainCommand};
    use crate::coverage::CoverageRecipe;
    use crate::daw_engine::DawEngineConfig;
    use crate::daw_render::PcmAsset;
    use crate::deprojection_program::{
        CandidateOrigin, Derivation, EditableTerm, EditableTermId, EditableTermKind, MaterialSpan,
        SourceClaim, SourceProgram, StructuralScorePolicy,
    };
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::ontology::{Producer, Provenance};
    use crate::project_audio_controller::{
        ProjectAudioControllerEffect, ProjectAudioPlanStamp, ProjectAudioRenderRecipe,
    };
    use crate::project_session::{ProjectSession, ProjectSessionId};
    use crate::render_plan::{DeterminismGrade, ExactDigest, RenderSpan, Tileability};
    use crate::rhythm::SampleSpan;
    use crate::rhythm_explanation::PatternAlternativeId;

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Analyzer {
                name: "artifact-promotion-bridge-test".into(),
                version: "1".into(),
                configuration_digest: None,
            },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn fixture() -> (ProjectSession, crate::assets::AssetId, Vec<f32>) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/artifact-promotion-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = crate::assets::AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "distinctive source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 8_000,
                    channels: 1,
                    frame_count: SampleFrames(8),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"distinctive bridge source"),
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
        let samples = vec![0.125, -0.75, 0.375, 0.9, -0.2, 0.55, -0.95, 0.3];
        let pcm = PcmAsset::new(
            AudioFormat::new(8_000, 1).unwrap(),
            Arc::from(samples.clone()),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Bridge", "Distinctive source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let source_clip = live.primary_source_ids().unwrap().clip;
        let mut session = ProjectSession::new(ProjectSessionId(301)).unwrap();
        session.install(live, None).unwrap();

        // Keep the exact source asset available for comparison, but remove the
        // imported clip from the audible schedule. The promoted construction
        // is then the sole scheduled input and satisfies today's authoritative
        // exclusive-isolation contract.
        let before = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .arrangement
            .clip(source_clip)
            .unwrap()
            .clone();
        let mut after = before.clone();
        after.muted = true;
        let commands = vec![DomainCommand::Arrangement(ArrangementOperation::PutClip {
            before: Some(before),
            after: Some(after),
        })];
        session
            .execute_envelope(CommandEnvelope {
                label: "Mute imported comparison reference".into(),
                base_revision: session.project_snapshot().unwrap().revisions().aggregate,
                coalesce: None,
                id_claims: claims_for_commands(&commands),
                commands,
            })
            .unwrap();
        (session, asset, samples)
    }

    #[test]
    fn distinctive_pcm_promotion_changes_residual_and_one_undo_removes_it() {
        let (mut session, asset, samples) = fixture();
        let revisions = session.project_snapshot().unwrap().revisions();
        let publication_generation = session.snapshot().generation;
        let descriptor = ArtifactDescriptor {
            id: ArtifactId(digest(0x44)),
            kind: crate::artifact_catalog::ArtifactKind::ModelClaim,
            source_digest: digest(0x11),
            recipe_digest: digest(0x22),
            output_digest: digest(0x44),
            extent: FrameSpan::new(0, 8).unwrap(),
            sample_rate: 8_000,
            channels: 1,
            provenance: provenance(),
        };
        let pin = ArtifactComparisonPin::from_descriptor(
            &descriptor,
            revisions,
            publication_generation,
            1,
        );
        let payload = ArtifactComparisonPayload::from_model_render(
            &descriptor,
            pin,
            77,
            crate::explanation::RenderedExplanation {
                origin_frame: 0,
                audio: ProjectAudio::from_interleaved(
                    AudioFormat::new(8_000, 1).unwrap(),
                    samples.clone(),
                )
                .unwrap(),
            },
            &RenderCancellation::new(),
        )
        .unwrap();
        let mut catalog = ArtifactCatalog::new();
        insert_artifact_comparison_payload(&mut catalog, descriptor.clone(), Arc::new(payload))
            .unwrap();

        let material = MaterialSpan {
            material_sha256: "11".repeat(32),
            start_frame: 0,
            frame_count: 8,
            sample_rate_hz: 8_000,
            channels: 1,
        };
        let claim = SourceClaim::literal(material.clone(), descriptor.source_digest).unwrap();
        let term = EditableTerm {
            id: EditableTermId(digest(0x33)),
            kind: EditableTermKind::ExactAudioReference {
                source: claim.id,
                span: SampleSpan { start: 0, end: 8 },
            },
            evidence: vec![
                EvidenceRef::Artifact(descriptor.id),
                EvidenceRef::SourceClaim(claim.id),
            ],
            derivation: Derivation {
                rule: "test.artifact-exact-audio.v1".into(),
                recipe: descriptor.recipe_digest,
                premises: vec![
                    EvidenceRef::Artifact(descriptor.id),
                    EvidenceRef::SourceClaim(claim.id),
                ],
            },
            description_bytes: 64,
            free_parameters: 0,
        };
        let program = SourceProgram::new(material, vec![term.clone()], vec![term.id]).unwrap();
        let candidate = DeprojectionCandidate::new(
            "Artifact-backed exact construction".into(),
            CandidateOrigin::NativeRhythm {
                alternative: PatternAlternativeId(digest(0x55)),
            },
            program,
            vec![claim.clone()],
            1_000_000,
            StructuralScorePolicy::default(),
            vec!["Exact audio fallback; no physical source identity asserted".into()],
        )
        .unwrap();
        let request = ArtifactPromotionComparisonRequest {
            artifact: descriptor.id,
            artifact_pin: pin,
            expected_document_generation: session.document_generation(),
            candidate,
            bindings: PromotionBindings {
                source_assets: BTreeMap::from([(
                    claim.id,
                    crate::deprojection_execution::promotion::ResolvedSourceAsset {
                        asset,
                        claim_frame_zero: 0,
                        frame_count: 8,
                    },
                )]),
                ..PromotionBindings::default()
            },
            placement: PromotionPlacement::default(),
            target: ArtifactPromotionComparisonTarget {
                comparison: ComparisonId(901),
                explanation: ExplanationId(902),
                label: "Promoted artifact construction".into(),
                source: SourceCitation {
                    asset,
                    source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(8)).unwrap(),
                    project_span: FrameSpan::new(0, 8).unwrap(),
                    channels: ChannelMask(1),
                },
                provenance: provenance(),
            },
            recipe: ComparisonProductRecipe {
                coverage: CoverageRecipe {
                    fft_size: 4,
                    hop_size: 2,
                    power_floor: 1.0e-12,
                },
                ..ComparisonProductRecipe::default()
            },
        };
        let cancellation = RenderCancellation::new();
        let result = plan_artifact_promotion_comparison(&session, &catalog, request, &cancellation)
            .unwrap()
            .execute(&mut session, &cancellation)
            .unwrap();
        assert!(result.promotion.provenance[&term.id]
            .evidence
            .contains(&EvidenceRef::Artifact(descriptor.id)));
        let promoted_clip = result
            .promotion
            .created
            .iter()
            .find_map(|object| match object {
                CreatedObject::ExactAudioFallbackClip(clip) => Some(*clip),
                _ => None,
            })
            .unwrap();

        let mut audio = ProjectAudioController::new();
        audio.set_tile_policy(None);
        let render = result
            .request_shared_render(
                &session,
                &mut audio,
                ProjectAudioRenderRecipe {
                    extent: RenderSpan::new(0, 8).unwrap(),
                    engine: Arc::new(DawEngineConfig {
                        output_channels: 1,
                        block_frames: 4,
                        ..DawEngineConfig::default()
                    }),
                    stamp: ProjectAudioPlanStamp {
                        project_namespace: 301,
                        snapshot: ExactDigest::new([0x61; 32]),
                        engine_abi: 1,
                        engine_configuration: ExactDigest::new([0x62; 32]),
                        dependencies: Vec::new(),
                        determinism: DeterminismGrade::BitExact,
                        tileability: Tileability::Stateless,
                    },
                },
                &cancellation,
            )
            .unwrap();
        let completion = render.execute(&cancellation).unwrap();
        assert!(matches!(
            audio.complete_render(completion).unwrap(),
            ProjectAudioControllerEffect::OpenHost(_)
        ));

        let mut controller = ComparisonController::new(71).unwrap();
        let mut executor = ComparisonProductExecutor::new();
        let capture = result
            .capture_updated_comparison(
                &session,
                &audio,
                &mut controller,
                &mut executor,
                ComparisonChannel::Residual,
                &cancellation,
            )
            .unwrap();
        let comparison = capture.job.execute().unwrap();
        assert_eq!(comparison.execution.rendered.source.interleaved(), samples);
        assert_eq!(
            comparison.execution.rendered.construction.interleaved(),
            samples
        );
        assert!(comparison
            .execution
            .rendered
            .residual
            .interleaved()
            .iter()
            .all(|sample| sample.abs() <= f32::EPSILON));
        assert_ne!(
            comparison.execution.rendered.residual.interleaved(),
            comparison.execution.rendered.source.interleaved(),
            "the atomic promotion edit must change the source-only residual"
        );

        result.undo(&mut session).unwrap();
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .arrangement
            .clip(promoted_clip)
            .is_none());
    }
}
