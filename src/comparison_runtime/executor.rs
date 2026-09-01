//! Cancellable execution bridge for live reverse-surface comparisons.
//!
//! Capture is deliberately strict: a job is admitted only when the project
//! session, selection request, and the active shared-renderer executable all
//! name the same complete project revision and the same span. The job then
//! resolves the source from the frozen publication and renders constructive
//! DAW explanations through that executable's already-compiled
//! [`DawEngineSchedule`](crate::daw_engine::DawEngineSchedule). It never opens
//! an audio device, creates a transport, fabricates PCM, or compiles a second
//! engine schedule.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::{sha256_content, ArtifactCatalog};
use crate::comparison::{ComparisonDefinition, ComparisonObservation};
use crate::comparison_controller::{
    ComparisonAudioEffect, ComparisonAuditionProducts, ComparisonController,
    ComparisonControllerError, ComparisonDigestPins, ComparisonSelectionRequest,
};
use crate::comparison_runtime::{
    ComparisonExecution, ComparisonRuntime, ComparisonRuntimeError, PcmComparisonSourceResolver,
};
use crate::coverage::CoverageRecipe;
use crate::daw_project::ProjectRevisions;
use crate::daw_render::RenderCancellation;
use crate::explanation_adapters::{
    CatalogAnalysisResolver, ExclusiveScheduleIsolationBackend, ResolvingExplanationCompiler,
    ScheduleDawScopeResolver,
};
use crate::interpretation::InterpretationStore;
use crate::project_audio_controller::ProjectAudioController;
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::render_plan::ExactDigest;
use crate::render_runtime::{project_revision_stamp, AuditionOwner, ExecutableRenderPlan};

const PRODUCT_BOUNDARY_DOMAIN: &[u8] = b"audec:comparison-product-boundary:v1";

/// Immutable semantic inputs which may be shared with a background job.
/// Callers replace these Arcs when interpretation or artifact truth changes;
/// an in-flight job continues to see the exact snapshot it captured.
#[derive(Clone)]
pub struct ComparisonSemanticSnapshot {
    pub interpretations: Arc<InterpretationStore>,
    pub artifacts: Arc<ArtifactCatalog>,
}

impl fmt::Debug for ComparisonSemanticSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComparisonSemanticSnapshot")
            .field("interpretations", &self.interpretations)
            .field("artifacts", &self.artifacts)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComparisonProductRecipe {
    pub coverage: CoverageRecipe,
    pub boundary_recipe: ExactDigest,
}

impl Default for ComparisonProductRecipe {
    fn default() -> Self {
        Self {
            coverage: CoverageRecipe::default(),
            boundary_recipe: ExactDigest::new(sha256_content(PRODUCT_BOUNDARY_DOMAIN, &[]).bytes),
        }
    }
}

#[derive(Clone, Debug)]
struct ActiveRequest {
    generation: u64,
    document_generation: u64,
    revisions: ProjectRevisions,
    cancellation: RenderCancellation,
}

/// Control-thread owner of per-pane cancellation and supersession state.
#[derive(Debug, Default)]
pub struct ComparisonProductExecutor {
    active: BTreeMap<AuditionOwner, ActiveRequest>,
}

impl ComparisonProductExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture a worker-safe job from the one session snapshot and the one
    /// active render executable. Selecting another generation for `owner`
    /// cancels its older job before any potentially failing resolution work.
    pub fn capture(
        &mut self,
        owner: AuditionOwner,
        request: ComparisonSelectionRequest,
        session: &ProjectSession,
        audio: &ProjectAudioController,
        semantics: ComparisonSemanticSnapshot,
        recipe: ComparisonProductRecipe,
    ) -> Result<ComparisonProductJob, ComparisonProductExecutorError> {
        if let Some(active) = self.active.get(&owner) {
            if request.generation <= active.generation {
                return Err(ComparisonProductExecutorError::Superseded {
                    completed: request.generation,
                    desired: Some(active.generation),
                });
            }
        }
        self.cancel_owner(owner);

        let snapshot = session.project_snapshot()?.clone();
        let current = snapshot.revisions();
        require_revision(request.requested_at, current)?;

        let definition = semantics
            .interpretations
            .comparison(request.comparison)
            .cloned()
            .ok_or(ComparisonProductExecutorError::MissingComparison(
                request.comparison,
            ))?;
        validate_request_definition(&request, &definition)?;
        let recorded_observation = semantics
            .interpretations
            .observation(request.comparison)
            .cloned()
            .ok_or(ComparisonProductExecutorError::MissingObservation(
                request.comparison,
            ))?;
        if ComparisonDigestPins::from(&recorded_observation) != request.digests {
            return Err(ComparisonProductExecutorError::ObservationChanged);
        }

        let active_cohort = audio
            .runtime()
            .service()
            .active_cohort()
            .ok_or(ComparisonProductExecutorError::NoActiveRender)?;
        let executable = audio
            .runtime()
            .executable_plan(&active_cohort.id.plan)
            .map_err(|error| ComparisonProductExecutorError::Render(error.to_string()))?;
        let active_revision = executable.schedule.project_revision();
        require_revision(request.requested_at, active_revision).map_err(|_| {
            ComparisonProductExecutorError::ActiveRenderRevisionMismatch {
                requested: request.requested_at,
                active: active_revision,
            }
        })?;
        if executable.id().revisions != project_revision_stamp(request.requested_at) {
            return Err(ComparisonProductExecutorError::ActiveRenderStampMismatch);
        }
        if !executable.descriptor.extent().contains_span(request.span) {
            return Err(ComparisonProductExecutorError::SpanOutsideActiveRender {
                span: request.span,
                active: executable.descriptor.extent(),
            });
        }

        let cancellation = RenderCancellation::new();
        self.active.insert(
            owner,
            ActiveRequest {
                generation: request.generation,
                document_generation: session.document_generation(),
                revisions: request.requested_at,
                cancellation: cancellation.clone(),
            },
        );
        Ok(ComparisonProductJob {
            owner,
            document_generation: session.document_generation(),
            request,
            definition,
            recorded_observation,
            project: snapshot,
            executable,
            semantics,
            recipe,
            cancellation,
        })
    }

    /// Cancel a pane's outstanding comparison without touching the shared
    /// project render, transport, or another pane's owner.
    pub fn cancel_owner(&mut self, owner: AuditionOwner) -> bool {
        let Some(active) = self.active.remove(&owner) else {
            return false;
        };
        active.cancellation.cancel();
        true
    }

    pub fn is_current(&self, owner: AuditionOwner, generation: u64) -> bool {
        self.active
            .get(&owner)
            .is_some_and(|active| active.generation == generation)
    }

    /// Short main-thread publication boundary. It rechecks session revision
    /// and owner/generation immediately before delegating the already
    /// validated products to `ComparisonController`.
    pub fn publish(
        &mut self,
        session: &ProjectSession,
        controller: &mut ComparisonController,
        completion: ComparisonProductCompletion,
    ) -> Result<PublishedComparisonProducts, ComparisonProductExecutorError> {
        if controller.owner() != completion.owner {
            return Err(ComparisonProductExecutorError::OwnerMismatch {
                expected: controller.owner(),
                actual: completion.owner,
            });
        }
        let Some(active) = self.active.get(&completion.owner) else {
            return Err(ComparisonProductExecutorError::Superseded {
                completed: completion.generation,
                desired: None,
            });
        };
        if active.generation != completion.generation {
            return Err(ComparisonProductExecutorError::Superseded {
                completed: completion.generation,
                desired: Some(active.generation),
            });
        }
        if active.cancellation.is_cancelled() || completion.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        let current_document = session.document_generation();
        if active.document_generation != current_document
            || completion.document_generation != current_document
        {
            return Err(ComparisonProductExecutorError::DocumentSuperseded {
                captured: completion.document_generation,
                current: current_document,
            });
        }
        let current = session.project_snapshot()?.revisions();
        require_revision(active.revisions, current)?;
        if completion.producing_revision != current {
            return Err(ComparisonProductExecutorError::StaleRevision {
                requested: completion.producing_revision,
                current,
            });
        }
        let effect = controller
            .accept_products(&completion.request, Arc::clone(&completion.products))
            .map_err(ComparisonProductExecutorError::Controller)?;
        self.active.remove(&completion.owner);
        Ok(PublishedComparisonProducts {
            effect,
            execution: completion.execution,
            products: completion.products,
        })
    }
}

/// Frozen background work. The cancellation token is intentionally exposed so
/// a task owner can cancel it without inventing another cancellation domain.
#[derive(Clone)]
pub struct ComparisonProductJob {
    owner: AuditionOwner,
    document_generation: u64,
    request: ComparisonSelectionRequest,
    definition: ComparisonDefinition,
    recorded_observation: ComparisonObservation,
    project: crate::live_project::LiveProjectSnapshot,
    executable: Arc<ExecutableRenderPlan>,
    semantics: ComparisonSemanticSnapshot,
    recipe: ComparisonProductRecipe,
    cancellation: RenderCancellation,
}

impl fmt::Debug for ComparisonProductJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComparisonProductJob")
            .field("owner", &self.owner)
            .field("request", &self.request)
            .field("producing_plan", self.executable.id())
            .finish_non_exhaustive()
    }
}

impl ComparisonProductJob {
    pub const fn owner(&self) -> AuditionOwner {
        self.owner
    }

    pub const fn generation(&self) -> u64 {
        self.request.generation
    }

    pub fn request(&self) -> &ComparisonSelectionRequest {
        &self.request
    }

    pub fn cancellation(&self) -> RenderCancellation {
        self.cancellation.clone()
    }

    /// Resolve and render from frozen inputs. DAW-backed explanations use the
    /// active executable schedule; artifact-backed scopes use immutable
    /// content-addressed payloads from the semantic snapshot.
    pub fn execute(&self) -> Result<ComparisonProductCompletion, ComparisonProductExecutorError> {
        if self.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        let daw = ScheduleDawScopeResolver::new(
            &self.project.project,
            Arc::clone(&self.executable.schedule),
            Arc::new(ExclusiveScheduleIsolationBackend),
        )
        .map_err(|error| ComparisonProductExecutorError::Explanation(error.to_string()))?;
        let analysis = CatalogAnalysisResolver {
            catalog: &self.semantics.artifacts,
        };
        let compiler = ResolvingExplanationCompiler {
            revisions: self.project.revisions(),
            definitions: self.semantics.interpretations.as_ref(),
            daw: &daw,
            analysis: &analysis,
        };
        let source = PcmComparisonSourceResolver {
            assets: &self.project.project.state().domains.assets,
            pcm: &self.project.pcm,
        };
        let runtime = ComparisonRuntime {
            interpretations: &self.semantics.interpretations,
            explanations: &compiler,
            sources: &source,
        };
        let execution =
            match runtime.execute(&self.definition, self.recipe.coverage, &self.cancellation) {
                Ok(execution) => execution,
                Err(_) if self.cancellation.is_cancelled() => {
                    return Err(ComparisonProductExecutorError::Cancelled);
                }
                Err(error) => return Err(map_runtime_error(error)),
            };
        if execution.observation != self.recorded_observation {
            return Err(ComparisonProductExecutorError::ObservationChanged);
        }
        if self.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        let products = execution
            .render_products(self.executable.id(), self.recipe.boundary_recipe)
            .map_err(ComparisonProductExecutorError::Runtime)?;
        let products = Arc::new(
            ComparisonAuditionProducts::validate(
                &self.definition,
                execution.observation.clone(),
                products,
            )
            .map_err(ComparisonProductExecutorError::Controller)?,
        );
        if self.cancellation.is_cancelled() {
            return Err(ComparisonProductExecutorError::Cancelled);
        }
        Ok(ComparisonProductCompletion {
            owner: self.owner,
            generation: self.request.generation,
            document_generation: self.document_generation,
            producing_revision: self.project.revisions(),
            request: self.request.clone(),
            execution,
            products,
            cancellation: self.cancellation.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ComparisonProductCompletion {
    pub owner: AuditionOwner,
    pub generation: u64,
    pub document_generation: u64,
    pub producing_revision: ProjectRevisions,
    pub request: ComparisonSelectionRequest,
    pub execution: ComparisonExecution,
    pub products: Arc<ComparisonAuditionProducts>,
    cancellation: RenderCancellation,
}

#[derive(Clone, Debug)]
pub struct PublishedComparisonProducts {
    pub effect: ComparisonAudioEffect,
    pub execution: ComparisonExecution,
    pub products: Arc<ComparisonAuditionProducts>,
}

fn validate_request_definition(
    request: &ComparisonSelectionRequest,
    definition: &ComparisonDefinition,
) -> Result<(), ComparisonProductExecutorError> {
    if definition.id != request.comparison
        || definition.explanation != request.explanation
        || definition.source.project_span.start != request.span.start
        || definition.source.project_span.end != request.span.end
    {
        return Err(ComparisonProductExecutorError::DefinitionChanged);
    }
    definition
        .validate()
        .map_err(|error| ComparisonProductExecutorError::Definition(error.to_string()))
}

fn require_revision(
    requested: ProjectRevisions,
    current: ProjectRevisions,
) -> Result<(), ComparisonProductExecutorError> {
    if requested == current {
        Ok(())
    } else {
        Err(ComparisonProductExecutorError::StaleRevision { requested, current })
    }
}

fn map_runtime_error(error: ComparisonRuntimeError) -> ComparisonProductExecutorError {
    if matches!(
        error,
        ComparisonRuntimeError::Cancelled
            | ComparisonRuntimeError::Explanation(crate::explanation::ExplanationError::Cancelled)
            | ComparisonRuntimeError::Coverage(crate::coverage::CoverageError::Cancelled)
    ) {
        ComparisonProductExecutorError::Cancelled
    } else {
        ComparisonProductExecutorError::Runtime(error)
    }
}

#[derive(Debug)]
pub enum ComparisonProductExecutorError {
    Cancelled,
    NoActiveRender,
    MissingComparison(crate::comparison::ComparisonId),
    MissingObservation(crate::comparison::ComparisonId),
    DefinitionChanged,
    ObservationChanged,
    StaleRevision {
        requested: ProjectRevisions,
        current: ProjectRevisions,
    },
    ActiveRenderRevisionMismatch {
        requested: ProjectRevisions,
        active: ProjectRevisions,
    },
    ActiveRenderStampMismatch,
    SpanOutsideActiveRender {
        span: crate::render_plan::RenderSpan,
        active: crate::render_plan::RenderSpan,
    },
    OwnerMismatch {
        expected: AuditionOwner,
        actual: AuditionOwner,
    },
    Superseded {
        completed: u64,
        desired: Option<u64>,
    },
    DocumentSuperseded {
        captured: u64,
        current: u64,
    },
    Definition(String),
    Explanation(String),
    Render(String),
    Runtime(ComparisonRuntimeError),
    Controller(ComparisonControllerError),
    Session(ProjectSessionError),
}

impl fmt::Display for ComparisonProductExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "comparison product executor: {self:?}")
    }
}

impl Error for ComparisonProductExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::Controller(error) => Some(error),
            Self::Session(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProjectSessionError> for ComparisonProductExecutorError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::aspect::{Aspect, ChannelMask, FrameSpan};
    use crate::assets::{
        AbsolutePath, AssetFrameRange, AssetLocation, AssetOrigin, AssetProvenance,
        AssetRegistration, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::comparison::{ComparisonId, SourceCitation};
    use crate::comparison_controller::ComparisonChannel;
    use crate::daw_engine::DawEngineConfig;
    use crate::daw_render::PcmAsset;
    use crate::explanation::{ExplanationDefinition, ExplanationId, ExplanationScope};
    use crate::interpretation::InterpretationCommand;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::ontology::{Producer, Provenance};
    use crate::project_audio_controller::{
        ProjectAudioControllerEffect, ProjectAudioPlanStamp, ProjectAudioRenderRecipe,
    };
    use crate::project_session::{ProjectPublication, ProjectSessionId};
    use crate::render_plan::{DeterminismGrade, RenderSpan, Tileability};

    struct Fixture {
        session: ProjectSession,
        audio: ProjectAudioController,
        semantics: ComparisonSemanticSnapshot,
        definition: ComparisonDefinition,
        observation: ComparisonObservation,
    }

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn recipe() -> ComparisonProductRecipe {
        ComparisonProductRecipe {
            coverage: CoverageRecipe {
                fft_size: 2,
                hop_size: 1,
                power_floor: 1.0e-12,
            },
            ..ComparisonProductRecipe::default()
        }
    }

    fn fixture() -> Fixture {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/comparison-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = crate::assets::AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "comparison source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 8_000,
                    channels: 2,
                    frame_count: SampleFrames(4),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"comparison source"),
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
            AudioFormat::new(8_000, 2).unwrap(),
            Arc::from([0.25, -0.25, 0.5, -0.5, 0.75, -0.75, 1.0, -1.0]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("comparison", "source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let source_ids = live.primary_source_ids().unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(91)).unwrap();
        let revisions = session.install(live, None).unwrap();
        let snapshot = session.project_snapshot().unwrap().clone();
        let publication = ProjectPublication {
            generation: session.snapshot().generation,
            revisions,
            snapshot: snapshot.clone(),
            change_set: None,
        };
        let mut audio = ProjectAudioController::new();
        audio.set_tile_policy(None);
        let render_job = audio.request_render(
            publication,
            ProjectAudioRenderRecipe {
                extent: RenderSpan::new(0, 4).unwrap(),
                engine: Arc::new(DawEngineConfig {
                    output_channels: 2,
                    block_frames: 2,
                    ..DawEngineConfig::default()
                }),
                stamp: ProjectAudioPlanStamp {
                    project_namespace: 91,
                    snapshot: ExactDigest::new([1; 32]),
                    engine_abi: 1,
                    engine_configuration: ExactDigest::new([2; 32]),
                    dependencies: Vec::new(),
                    determinism: DeterminismGrade::BitExact,
                    tileability: Tileability::Stateless,
                },
            },
        );
        let completion = render_job.execute(&RenderCancellation::new()).unwrap();
        assert!(matches!(
            audio.complete_render(completion).unwrap(),
            ProjectAudioControllerEffect::OpenHost(_)
        ));

        let explanation = ExplanationDefinition {
            id: ExplanationId(7),
            label: "only source clip".into(),
            scope: ExplanationScope::ArrangementClip(source_ids.clip),
            extent: Aspect::Time(FrameSpan::new(0, 4).unwrap()),
            evidence: Vec::new(),
            provenance: provenance(),
        };
        let definition = ComparisonDefinition {
            id: ComparisonId(8),
            label: "source against construction".into(),
            source: SourceCitation {
                asset,
                source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(4)).unwrap(),
                project_span: FrameSpan::new(0, 4).unwrap(),
                channels: ChannelMask(0b11),
            },
            explanation: explanation.id,
            provenance: provenance(),
        };
        let mut interpretations = InterpretationStore::new();
        interpretations
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(explanation),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(definition.clone()),
                },
            ])
            .unwrap();

        let executable = {
            let cohort = audio.runtime().service().active_cohort().unwrap();
            audio.runtime().executable_plan(&cohort.id.plan).unwrap()
        };
        let artifacts = Arc::new(ArtifactCatalog::new());
        let daw = ScheduleDawScopeResolver::new(
            &snapshot.project,
            Arc::clone(&executable.schedule),
            Arc::new(ExclusiveScheduleIsolationBackend),
        )
        .unwrap();
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
        let observation = ComparisonRuntime {
            interpretations: &interpretations,
            explanations: &compiler,
            sources: &source,
        }
        .execute(&definition, recipe().coverage, &RenderCancellation::new())
        .unwrap()
        .observation;
        interpretations
            .apply(&[InterpretationCommand::PutObservation {
                comparison: definition.id,
                before: None,
                after: Some(observation.clone()),
            }])
            .unwrap();
        Fixture {
            session,
            audio,
            semantics: ComparisonSemanticSnapshot {
                interpretations: Arc::new(interpretations),
                artifacts,
            },
            definition,
            observation,
        }
    }

    fn select(
        fixture: &Fixture,
        controller: &mut ComparisonController,
        channel: ComparisonChannel,
    ) -> ComparisonSelectionRequest {
        controller
            .select(
                &fixture.definition,
                &fixture.observation,
                fixture.session.project_snapshot().unwrap().revisions(),
                channel,
            )
            .unwrap()
    }

    #[test]
    fn exact_revision_job_preserves_alignment_and_exact_unfitted_residual() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(12).unwrap();
        let request = select(&fixture, &mut controller, ComparisonChannel::Source);
        let mut executor = ComparisonProductExecutor::new();
        let completion = executor
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap()
            .execute()
            .unwrap();
        let rendered = &completion.execution.rendered;
        assert_eq!(rendered.origin_frame, 0);
        assert_eq!(
            rendered.source.frame_count(),
            rendered.construction.frame_count()
        );
        assert_eq!(rendered.source.format(), rendered.construction.format());
        for ((source, construction), residual) in rendered
            .source
            .interleaved()
            .iter()
            .zip(rendered.construction.interleaved())
            .zip(rendered.residual.interleaved())
        {
            assert_eq!(residual.to_bits(), (source - construction).to_bits());
        }
        assert_eq!(
            completion.execution.coverage.origin_frame,
            rendered.origin_frame
        );
        assert_eq!(
            completion.execution.coverage.frame_count,
            rendered.source.frame_count().0
        );
        assert_eq!(
            completion
                .products
                .product(ComparisonChannel::Source)
                .unwrap()
                .interleaved(),
            rendered.source.interleaved()
        );

        let published = executor
            .publish(&fixture.session, &mut controller, completion)
            .unwrap();
        let ComparisonAudioEffect::Publish { audition, .. } = published.effect else {
            panic!("source is a time-domain publication")
        };
        assert_eq!(
            audition.interleaved(),
            published.execution.rendered.source.interleaved()
        );
    }

    #[test]
    fn owner_supersession_cancels_work_and_rejects_a_late_completion() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(13).unwrap();
        let mut executor = ComparisonProductExecutor::new();
        let first_request = select(&fixture, &mut controller, ComparisonChannel::Source);
        let first = executor
            .capture(
                controller.owner(),
                first_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        let first_completion = first.execute().unwrap();
        let second_request = select(&fixture, &mut controller, ComparisonChannel::Residual);
        let second = executor
            .capture(
                controller.owner(),
                second_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        assert!(first.cancellation().is_cancelled());
        assert!(matches!(
            executor.publish(&fixture.session, &mut controller, first_completion),
            Err(ComparisonProductExecutorError::Superseded { .. })
        ));
        assert!(second.execute().is_ok());

        let third_request = select(&fixture, &mut controller, ComparisonChannel::Construction);
        let cancelled = executor
            .capture(
                controller.owner(),
                third_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        let fourth_request = select(&fixture, &mut controller, ComparisonChannel::Source);
        let _fourth = executor
            .capture(
                controller.owner(),
                fourth_request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap();
        assert!(matches!(
            cancelled.execute(),
            Err(ComparisonProductExecutorError::Cancelled)
        ));
    }

    #[test]
    fn stale_capture_is_refused_before_rendering() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(14).unwrap();
        let mut stale = fixture.session.project_snapshot().unwrap().revisions();
        stale.aggregate = stale.aggregate.saturating_sub(1);
        let request = controller
            .select(
                &fixture.definition,
                &fixture.observation,
                stale,
                ComparisonChannel::Source,
            )
            .unwrap();
        let error = ComparisonProductExecutor::new()
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics,
                recipe(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ComparisonProductExecutorError::StaleRevision { .. }
        ));
    }

    #[test]
    fn publication_rechecks_the_completion_revision() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(16).unwrap();
        let request = select(&fixture, &mut controller, ComparisonChannel::Residual);
        let mut executor = ComparisonProductExecutor::new();
        let mut completion = executor
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics,
                recipe(),
            )
            .unwrap()
            .execute()
            .unwrap();
        completion.producing_revision.aggregate =
            completion.producing_revision.aggregate.saturating_sub(1);
        assert!(matches!(
            executor.publish(&fixture.session, &mut controller, completion),
            Err(ComparisonProductExecutorError::StaleRevision { .. })
        ));
    }

    #[test]
    fn excess_remains_spectral_and_clears_pcm_for_its_owner() {
        let fixture = fixture();
        let mut controller = ComparisonController::new(15).unwrap();
        let request = select(&fixture, &mut controller, ComparisonChannel::Excess);
        let mut executor = ComparisonProductExecutor::new();
        let completion = executor
            .capture(
                controller.owner(),
                request,
                &fixture.session,
                &fixture.audio,
                fixture.semantics.clone(),
                recipe(),
            )
            .unwrap()
            .execute()
            .unwrap();
        assert!(completion
            .products
            .product(ComparisonChannel::Excess)
            .is_none());
        assert_eq!(
            completion.execution.coverage.excess.len(),
            completion.execution.coverage.explained.len()
        );
        let published = executor
            .publish(&fixture.session, &mut controller, completion)
            .unwrap();
        assert!(matches!(
            published.effect,
            ComparisonAudioEffect::Clear { .. }
        ));
    }
}
