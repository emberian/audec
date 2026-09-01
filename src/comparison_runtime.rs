//! Executable original/construction/residual comparison pipeline.
//!
//! The runtime freezes an explanation, resolves an exact source citation,
//! subtracts without fitting, computes coverage, and returns immutable products
//! plus the command-ready observation. Render-product conversion uses the same
//! canonical PCM identity as `RenderRuntime`, so products may be adopted by its
//! catalog without creating a second audio truth.

pub mod executor;

use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::{
    sha256_content, ArtifactCatalog, ArtifactDescriptor, ArtifactId, ArtifactKind, ContentDigest,
};
use crate::assets::{AssetAvailability, AssetId, AssetRegistry};
use crate::audio::{AudioFormat, ProjectAudio};
use crate::comparison::{
    render_comparison, ComparisonDefinition, ComparisonError, ComparisonId, ComparisonObservation,
    ExactRenderDigest, RenderedComparison, SourceCitation,
};
use crate::coverage::{compute_coverage, CoverageError, CoverageField, CoverageRecipe};
use crate::daw_engine::AssetPcmMap;
use crate::daw_render::RenderCancellation;
use crate::explanation::{
    CompiledExplanation, ExplanationCompiler, ExplanationError, ExplanationId,
};
use crate::interpretation::{InterpretationCommand, InterpretationStore};
use crate::interpretation_navigation::{rank_coverage_hotspots, CoverageHotspotDto};
use crate::ontology::Provenance;
use crate::render_plan::{
    ExactDigest, ExplanationScopeId, RenderFormat, RenderPlanId, RenderScope, RenderSpan,
};
use crate::render_products::{ProductPartition, RenderProduct, RenderProductKey};
use crate::render_runtime::canonical_pcm_digest;
use crate::render_validation::{GoldenFingerprint, Signal, SignalFormat};

const FINGERPRINT_ACTIVITY_EPSILON: f32 = 1.0e-6;
pub const COMPARISON_SOURCE_SCOPE_NAMESPACE: u128 = u128::from_be_bytes(*b"audec-source-v1!");
pub const COMPARISON_CONSTRUCTION_SCOPE_NAMESPACE: u128 = u128::from_be_bytes(*b"audec-constr-v1!");
pub const COMPARISON_RESIDUAL_SCOPE_NAMESPACE: u128 = u128::from_be_bytes(*b"audec-resid-v1!!");

#[derive(Clone, Debug)]
pub struct ResolvedComparisonSource {
    pub origin_frame: i64,
    pub audio: ProjectAudio,
}

pub trait ComparisonSourceResolver: Send + Sync {
    fn resolve_source(
        &self,
        citation: SourceCitation,
        cancellation: &RenderCancellation,
    ) -> Result<ResolvedComparisonSource, ComparisonRuntimeError>;
}

/// Exact, no-resampling resolver over a frozen media registry and decoder PCM
/// map. Selected channels are packed in ascending source-channel order.
pub struct PcmComparisonSourceResolver<'a> {
    pub assets: &'a AssetRegistry,
    pub pcm: &'a AssetPcmMap,
}

impl ComparisonSourceResolver for PcmComparisonSourceResolver<'_> {
    fn resolve_source(
        &self,
        citation: SourceCitation,
        cancellation: &RenderCancellation,
    ) -> Result<ResolvedComparisonSource, ComparisonRuntimeError> {
        if cancellation.is_cancelled() {
            return Err(ComparisonRuntimeError::Cancelled);
        }
        citation
            .validate()
            .map_err(ComparisonRuntimeError::Comparison)?;
        let asset = self
            .assets
            .get(citation.asset)
            .ok_or(ComparisonRuntimeError::MissingSourceAsset(citation.asset))?;
        if matches!(asset.availability(), AssetAvailability::Missing { .. }) {
            return Err(ComparisonRuntimeError::SourceAssetOffline(citation.asset));
        }
        let pcm = self
            .pcm
            .get(&citation.asset)
            .ok_or(ComparisonRuntimeError::MissingSourcePcm(citation.asset))?;
        let metadata = asset.metadata();
        if metadata.sample_rate_hz != pcm.format.sample_rate.get()
            || metadata.channels != pcm.format.channels.get()
            || metadata.frame_count.0 != pcm.frame_count()
        {
            return Err(ComparisonRuntimeError::SourceMetadataMismatch(
                citation.asset,
            ));
        }
        if !citation.source_range.is_within(metadata.frame_count) {
            return Err(ComparisonRuntimeError::SourceRangeOutsideAsset);
        }
        let source_frames = citation.source_range.len().0;
        let project_frames = u64::try_from(
            i128::from(citation.project_span.end) - i128::from(citation.project_span.start),
        )
        .map_err(|_| ComparisonRuntimeError::SourceTooLarge)?;
        if source_frames != project_frames {
            return Err(ComparisonRuntimeError::SourceResamplingRequired {
                source_frames,
                project_frames,
            });
        }
        let source_channels = usize::from(metadata.channels);
        if source_channels > 16 {
            return Err(ComparisonRuntimeError::InvalidSourceChannels {
                mask: citation.channels.0,
                available: metadata.channels,
            });
        }
        let selected = (0..source_channels)
            .filter(|channel| citation.channels.0 & (1_u16 << channel) != 0)
            .collect::<Vec<_>>();
        let valid_mask = if source_channels == 16 {
            u16::MAX
        } else {
            (1_u16 << source_channels) - 1
        };
        if selected.is_empty() || citation.channels.0 & !valid_mask != 0 {
            return Err(ComparisonRuntimeError::InvalidSourceChannels {
                mask: citation.channels.0,
                available: metadata.channels,
            });
        }
        let start = usize::try_from(citation.source_range.start.0)
            .ok()
            .and_then(|frame| frame.checked_mul(source_channels))
            .ok_or(ComparisonRuntimeError::SourceTooLarge)?;
        let end = usize::try_from(citation.source_range.end.0)
            .ok()
            .and_then(|frame| frame.checked_mul(source_channels))
            .ok_or(ComparisonRuntimeError::SourceTooLarge)?;
        let source = pcm
            .samples
            .get(start..end)
            .ok_or(ComparisonRuntimeError::SourceRangeOutsideAsset)?;
        let frames =
            usize::try_from(source_frames).map_err(|_| ComparisonRuntimeError::SourceTooLarge)?;
        let capacity = frames
            .checked_mul(selected.len())
            .ok_or(ComparisonRuntimeError::SourceTooLarge)?;
        let mut packed = Vec::with_capacity(capacity);
        for frame in source.chunks_exact(source_channels) {
            if cancellation.is_cancelled() {
                return Err(ComparisonRuntimeError::Cancelled);
            }
            for channel in &selected {
                packed.push(frame[*channel]);
            }
        }
        let format = AudioFormat::new(metadata.sample_rate_hz, selected.len() as u16)
            .map_err(|error| ComparisonRuntimeError::Audio(error.to_string()))?;
        Ok(ResolvedComparisonSource {
            origin_frame: citation.project_span.start,
            audio: ProjectAudio::from_interleaved(format, packed)
                .map_err(|error| ComparisonRuntimeError::Audio(error.to_string()))?,
        })
    }
}

pub struct ComparisonRuntime<'a> {
    pub interpretations: &'a InterpretationStore,
    pub explanations: &'a dyn ExplanationCompiler,
    pub sources: &'a dyn ComparisonSourceResolver,
}

impl ComparisonRuntime<'_> {
    pub fn execute(
        &self,
        definition: &ComparisonDefinition,
        recipe: CoverageRecipe,
        cancellation: &RenderCancellation,
    ) -> Result<ComparisonExecution, ComparisonRuntimeError> {
        definition
            .validate()
            .map_err(ComparisonRuntimeError::Comparison)?;
        let explanation_definition = self
            .interpretations
            .explanation(definition.explanation)
            .ok_or(ComparisonRuntimeError::MissingExplanation(
                definition.explanation,
            ))?;
        let compiled = self
            .explanations
            .compile(explanation_definition, cancellation)
            .map_err(ComparisonRuntimeError::Explanation)?;
        let source = self
            .sources
            .resolve_source(definition.source, cancellation)?;
        let construction = compiled
            .render(definition.source.project_span, cancellation)
            .map_err(ComparisonRuntimeError::Explanation)?;
        let rendered = render_comparison(source.origin_frame, source.audio, construction)
            .map_err(ComparisonRuntimeError::Comparison)?;
        let coverage = compute_coverage(&rendered, recipe, cancellation)
            .map_err(ComparisonRuntimeError::Coverage)?;
        let observation = observation(&rendered, compiled.dependencies().clone())?;
        Ok(ComparisonExecution {
            comparison: definition.id,
            explanation: definition.explanation,
            compiled,
            rendered,
            coverage,
            observation,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ComparisonExecution {
    pub comparison: ComparisonId,
    pub explanation: ExplanationId,
    pub compiled: CompiledExplanation,
    pub rendered: RenderedComparison,
    pub coverage: CoverageField,
    pub observation: ComparisonObservation,
}

impl ComparisonExecution {
    pub fn observation_command(
        &self,
        before: Option<ComparisonObservation>,
    ) -> InterpretationCommand {
        InterpretationCommand::PutObservation {
            comparison: self.comparison,
            before,
            after: Some(self.observation.clone()),
        }
    }

    pub fn hotspots(&self, limit: usize) -> Vec<CoverageHotspotDto> {
        rank_coverage_hotspots(&self.coverage, self.comparison, limit)
    }

    pub fn publish_coverage(
        &self,
        catalog: &mut ArtifactCatalog,
        provenance: Provenance,
    ) -> Result<ArtifactId, ComparisonRuntimeError> {
        let recipe_digest = coverage_recipe_digest(self.coverage.recipe);
        let output_digest = coverage_field_digest(&self.coverage);
        let id = ArtifactId(output_digest);
        let artifact = Arc::new(CoverageArtifact {
            comparison: self.comparison,
            source_digest: self.observation.source_digest,
            construction_digest: self.observation.construction_digest,
            residual_digest: self.observation.residual_digest,
            field: self.coverage.clone(),
        });
        catalog
            .insert(
                ArtifactDescriptor {
                    id,
                    kind: ArtifactKind::CoverageField,
                    source_digest: self.observation.source_digest.0,
                    recipe_digest,
                    output_digest,
                    extent: crate::aspect::FrameSpan {
                        start: self.coverage.origin_frame,
                        end: self
                            .coverage
                            .origin_frame
                            .checked_add(
                                i64::try_from(self.coverage.frame_count)
                                    .map_err(|_| ComparisonRuntimeError::SourceTooLarge)?,
                            )
                            .ok_or(ComparisonRuntimeError::SourceTooLarge)?,
                    },
                    sample_rate: self.coverage.sample_rate,
                    channels: self.coverage.channels,
                    provenance,
                },
                artifact,
            )
            .map_err(|error| ComparisonRuntimeError::Artifact(error.to_string()))?;
        Ok(id)
    }

    /// Create immutable products that `RenderRuntime::adopt_product` accepts.
    /// They deliberately use explanation scopes, not master, so comparison
    /// audition cannot be mistaken for the project's playback bounce.
    pub fn render_products(
        &self,
        plan: &RenderPlanId,
        boundary_recipe: ExactDigest,
    ) -> Result<ComparisonRenderProducts, ComparisonRuntimeError> {
        let span = RenderSpan::new(
            self.rendered.origin_frame,
            self.rendered
                .origin_frame
                .checked_add(
                    i64::try_from(self.rendered.source.frame_count().0)
                        .map_err(|_| ComparisonRuntimeError::SourceTooLarge)?,
                )
                .ok_or(ComparisonRuntimeError::SourceTooLarge)?,
        )
        .map_err(|error| ComparisonRuntimeError::Product(error.to_string()))?;
        if !plan.compiled_extent.contains_span(span) {
            return Err(ComparisonRuntimeError::ProductOutsidePlan);
        }
        let format = RenderFormat::new(
            self.rendered.source.format().sample_rate.get(),
            self.rendered.source.format().channels.get(),
        )
        .map_err(|error| ComparisonRuntimeError::Product(error.to_string()))?;
        if plan.engine.format != format {
            return Err(ComparisonRuntimeError::ProductFormatMismatch);
        }
        let partition = ProductPartition::ContiguousRun {
            anchor_frame: span.start,
            sequence: 0,
        };
        let source = render_product(
            plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_SOURCE_SCOPE_NAMESPACE,
                local: self.comparison.0,
            }),
            span,
            partition.clone(),
            boundary_recipe,
            &self.rendered.source,
        )?;
        let construction = render_product(
            plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_CONSTRUCTION_SCOPE_NAMESPACE,
                local: self.explanation.0,
            }),
            span,
            partition.clone(),
            boundary_recipe,
            &self.rendered.construction,
        )?;
        let residual = render_product(
            plan,
            RenderScope::Explanation(ExplanationScopeId {
                namespace: COMPARISON_RESIDUAL_SCOPE_NAMESPACE,
                local: self.comparison.0,
            }),
            span,
            partition,
            boundary_recipe,
            &self.rendered.residual,
        )?;
        Ok(ComparisonRenderProducts {
            source,
            construction,
            residual,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CoverageArtifact {
    pub comparison: ComparisonId,
    pub source_digest: ExactRenderDigest,
    pub construction_digest: ExactRenderDigest,
    pub residual_digest: ExactRenderDigest,
    pub field: CoverageField,
}

#[derive(Clone, Debug)]
pub struct ComparisonRenderProducts {
    pub source: Arc<RenderProduct>,
    pub construction: Arc<RenderProduct>,
    pub residual: Arc<RenderProduct>,
}

impl ComparisonRenderProducts {
    pub fn iter(&self) -> impl Iterator<Item = &Arc<RenderProduct>> {
        [&self.source, &self.construction, &self.residual].into_iter()
    }

    pub fn adopt_into(
        mut self,
        runtime: &mut crate::render_runtime::RenderRuntime,
    ) -> Result<Self, ComparisonRuntimeError> {
        self.source = runtime
            .adopt_product(self.source)
            .map_err(|error| ComparisonRuntimeError::Product(error.to_string()))?;
        self.construction = runtime
            .adopt_product(self.construction)
            .map_err(|error| ComparisonRuntimeError::Product(error.to_string()))?;
        self.residual = runtime
            .adopt_product(self.residual)
            .map_err(|error| ComparisonRuntimeError::Product(error.to_string()))?;
        Ok(self)
    }
}

fn render_product(
    plan: &RenderPlanId,
    scope: RenderScope,
    span: RenderSpan,
    partition: ProductPartition,
    boundary_recipe: ExactDigest,
    audio: &ProjectAudio,
) -> Result<Arc<RenderProduct>, ComparisonRuntimeError> {
    let key = RenderProductKey::new(plan.clone(), scope, span, partition, boundary_recipe)
        .map_err(|error| ComparisonRuntimeError::Product(error.to_string()))?;
    let digest = canonical_pcm_digest(audio.interleaved());
    RenderProduct::new(digest, key, audio.shared_interleaved())
        .map(Arc::new)
        .map_err(|error| ComparisonRuntimeError::Product(error.to_string()))
}

fn observation(
    rendered: &RenderedComparison,
    dependencies: crate::explanation::ExplanationDependencyPin,
) -> Result<ComparisonObservation, ComparisonRuntimeError> {
    let source_digest = exact_audio_digest(&rendered.source, rendered.origin_frame)?;
    let construction_digest = exact_audio_digest(&rendered.construction, rendered.origin_frame)?;
    let residual_digest = exact_audio_digest(&rendered.residual, rendered.origin_frame)?;
    let construction_fingerprint = golden(&rendered.construction, rendered.origin_frame)?;
    let residual_fingerprint = golden(&rendered.residual, rendered.origin_frame)?;
    Ok(ComparisonObservation {
        dependencies,
        source_digest,
        construction_digest,
        residual_digest,
        construction_fingerprint,
        residual_fingerprint,
        metrics: rendered.metrics,
    })
}

/// Canonical identity of one aligned comparison signal.  Kept distinct from
/// the PCM-only render-product digest because moving identical samples on the
/// project timeline changes the experiment they represent.
pub(crate) fn exact_audio_digest(
    audio: &ProjectAudio,
    origin_frame: i64,
) -> Result<ExactRenderDigest, ComparisonRuntimeError> {
    let mut pcm = Vec::with_capacity(audio.interleaved().len().saturating_mul(4));
    for sample in audio.interleaved() {
        pcm.extend_from_slice(&sample.to_bits().to_le_bytes());
    }
    let origin = origin_frame.to_le_bytes();
    let sample_rate = audio.format().sample_rate.get().to_le_bytes();
    let channels = audio.format().channels.get().to_le_bytes();
    let frames = audio.frame_count().0.to_le_bytes();
    ExactRenderDigest::new(sha256_content(
        b"audec:aligned-render-signal:v1",
        &[&origin, &sample_rate, &channels, &frames, &pcm],
    ))
    .map_err(ComparisonRuntimeError::Comparison)
}

fn golden(
    audio: &ProjectAudio,
    origin_frame: i64,
) -> Result<GoldenFingerprint, ComparisonRuntimeError> {
    let format = SignalFormat::new(
        audio.format().sample_rate.get(),
        audio.format().channels.get(),
    )
    .map_err(|error| ComparisonRuntimeError::Fingerprint(error.to_string()))?;
    let signal = Signal::new(format, origin_frame, audio.interleaved())
        .map_err(|error| ComparisonRuntimeError::Fingerprint(error.to_string()))?;
    Ok(GoldenFingerprint::from_signal(
        signal,
        FINGERPRINT_ACTIVITY_EPSILON,
    ))
}

fn coverage_recipe_digest(recipe: CoverageRecipe) -> ContentDigest {
    let fft = (recipe.fft_size as u64).to_le_bytes();
    let hop = (recipe.hop_size as u64).to_le_bytes();
    let floor = recipe.power_floor.to_bits().to_le_bytes();
    sha256_content(b"audec:coverage-recipe:v1", &[&fft, &hop, &floor])
}

fn coverage_field_digest(field: &CoverageField) -> ContentDigest {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&field.origin_frame.to_le_bytes());
    bytes.extend_from_slice(&field.sample_rate.to_le_bytes());
    bytes.extend_from_slice(&field.channels.to_le_bytes());
    bytes.extend_from_slice(&field.frame_count.to_le_bytes());
    bytes.extend_from_slice(&(field.recipe.fft_size as u64).to_le_bytes());
    bytes.extend_from_slice(&(field.recipe.hop_size as u64).to_le_bytes());
    bytes.extend_from_slice(&field.recipe.power_floor.to_bits().to_le_bytes());
    bytes.extend_from_slice(&(field.columns as u64).to_le_bytes());
    bytes.extend_from_slice(&(field.bins as u64).to_le_bytes());
    for values in [
        &field.source_power,
        &field.construction_power,
        &field.residual_power,
        &field.explained,
        &field.excess,
    ] {
        bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for value in values {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }
    for value in [
        field.summary.source_power,
        field.summary.construction_power,
        field.summary.residual_power,
        field.summary.signed_explained_energy,
        field.summary.clamped_explained_energy,
        field.summary.excess_energy_ratio,
    ] {
        bytes.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    sha256_content(b"audec:coverage-field:v1", &[&bytes])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComparisonRuntimeError {
    Cancelled,
    MissingExplanation(ExplanationId),
    MissingSourceAsset(AssetId),
    SourceAssetOffline(AssetId),
    MissingSourcePcm(AssetId),
    SourceMetadataMismatch(AssetId),
    SourceRangeOutsideAsset,
    SourceResamplingRequired {
        source_frames: u64,
        project_frames: u64,
    },
    InvalidSourceChannels {
        mask: u16,
        available: u16,
    },
    SourceTooLarge,
    ProductOutsidePlan,
    ProductFormatMismatch,
    Audio(String),
    Explanation(ExplanationError),
    Comparison(ComparisonError),
    Coverage(CoverageError),
    Fingerprint(String),
    Artifact(String),
    Product(String),
}

impl fmt::Display for ComparisonRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "comparison runtime error: {self:?}")
    }
}

impl std::error::Error for ComparisonRuntimeError {}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::artifact_catalog::ArtifactCatalog;
    use crate::aspect::{
        Aspect, BandSpan, ChannelMask, ConcreteAspect, ConcreteRegion, FrameSpan, SignalLayer,
    };
    use crate::assets::{
        AbsolutePath, AssetFrameRange, AssetLocation, AssetOrigin, AssetProvenance,
        AssetRegistration, ContentFingerprint, DecodedAudioMetadata, ProjectRelativePath,
        SampleFrames,
    };
    use crate::daw_project::ProjectRevisions;
    use crate::daw_render::PcmAsset;
    use crate::explanation::{
        ExplanationDefinition, ExplanationDependencyPin, ExplanationScope, PcmExplanationRenderer,
    };
    use crate::ontology::{Producer, Provenance};
    use crate::render_plan::{
        DeterminismGrade, EngineRecipeStamp, ProjectRevisionStamp, RenderPlan, Tileability,
    };

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn registry_and_pcm() -> (AssetRegistry, AssetPcmMap, AssetId) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/music/source.wav").unwrap()),
            Some(ProjectRelativePath::parse("media/source.wav").unwrap()),
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let id = registry
            .register(AssetRegistration {
                name: "source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 8_000,
                    channels: 2,
                    frame_count: SampleFrames(4),
                    container: Some("wav".into()),
                    codec: Some("pcm".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"source"),
                provenance: AssetProvenance::new(
                    0,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let format = AudioFormat::new(8_000, 2).unwrap();
        let pcm = BTreeMap::from([(
            id,
            PcmAsset::new(
                format,
                Arc::from([1.0, 10.0, 2.0, 20.0, 3.0, 30.0, 4.0, 40.0]),
            )
            .unwrap(),
        )]);
        (registry, pcm, id)
    }

    struct StaticCompiler(CompiledExplanation);
    impl ExplanationCompiler for StaticCompiler {
        fn compile(
            &self,
            _: &ExplanationDefinition,
            _: &RenderCancellation,
        ) -> Result<CompiledExplanation, ExplanationError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn exact_source_to_residual_to_coverage_to_render_products() {
        let (registry, pcm, asset) = registry_and_pcm();
        let definition = ExplanationDefinition {
            id: ExplanationId(1),
            label: "left channel".into(),
            scope: ExplanationScope::ArrangementClip(crate::arrangement::ClipId::from_raw(1)),
            extent: Aspect::Time(FrameSpan { start: 20, end: 23 }),
            evidence: Vec::new(),
            provenance: provenance(),
        };
        let extent = ConcreteAspect::new(
            vec![ConcreteRegion {
                time: FrameSpan { start: 20, end: 23 },
                band: BandSpan::new(0.0, 4_000.0).unwrap(),
                channels: ChannelMask(1),
            }],
            SignalLayer::Source,
        )
        .unwrap();
        let construction = ProjectAudio::from_interleaved(
            AudioFormat::new(8_000, 1).unwrap(),
            vec![2.0, 2.5, 4.0],
        )
        .unwrap();
        let compiled = CompiledExplanation::new(
            definition.clone(),
            extent,
            ExplanationDependencyPin::from_dependencies(ProjectRevisions::default(), [], []),
            Arc::new(PcmExplanationRenderer {
                origin_frame: 20,
                audio: construction,
            }),
        )
        .unwrap();
        let comparison = ComparisonDefinition {
            id: ComparisonId(1),
            label: "test".into(),
            source: SourceCitation {
                asset,
                source_range: AssetFrameRange::new(SampleFrames(1), SampleFrames(4)).unwrap(),
                project_span: FrameSpan { start: 20, end: 23 },
                channels: ChannelMask(1),
            },
            explanation: definition.id,
            provenance: provenance(),
        };
        let mut interpretations = InterpretationStore::new();
        interpretations
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(definition),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(comparison.clone()),
                },
            ])
            .unwrap();
        let source = PcmComparisonSourceResolver {
            assets: &registry,
            pcm: &pcm,
        };
        let runtime = ComparisonRuntime {
            interpretations: &interpretations,
            explanations: &StaticCompiler(compiled),
            sources: &source,
        };
        let execution = runtime
            .execute(
                &comparison,
                CoverageRecipe {
                    fft_size: 2,
                    hop_size: 1,
                    power_floor: 1.0e-12,
                },
                &RenderCancellation::new(),
            )
            .unwrap();
        assert_eq!(execution.rendered.source.interleaved(), &[2.0, 3.0, 4.0]);
        assert_eq!(
            execution.rendered.construction.interleaved(),
            &[2.0, 2.5, 4.0]
        );
        assert_eq!(execution.rendered.residual.interleaved(), &[0.0, 0.5, 0.0]);
        assert!(execution.observation.source_digest.0.is_strong());
        assert!(!execution.hotspots(2).is_empty());

        let mut artifacts = ArtifactCatalog::new();
        let coverage_id = execution
            .publish_coverage(&mut artifacts, provenance())
            .unwrap();
        assert_eq!(
            artifacts.descriptor(coverage_id).unwrap().kind,
            ArtifactKind::CoverageField
        );
        assert_eq!(
            artifacts
                .get::<CoverageArtifact>(coverage_id)
                .unwrap()
                .comparison,
            ComparisonId(1)
        );

        let format = RenderFormat::new(8_000, 1).unwrap();
        let engine = EngineRecipeStamp::new(1, format, 1, 0, ExactDigest::new([3; 32])).unwrap();
        let plan = RenderPlan::new(
            RenderPlanId::new(
                1,
                ExactDigest::new([4; 32]),
                ProjectRevisionStamp::default(),
                RenderSpan::new(20, 23).unwrap(),
                engine,
                Vec::new(),
            )
            .unwrap(),
            DeterminismGrade::BitExact,
            Tileability::Stateless,
        );
        let products = execution
            .render_products(&plan.id, ExactDigest::new([5; 32]))
            .unwrap();
        assert_eq!(products.source.interleaved(), &[2.0, 3.0, 4.0]);
        assert_eq!(products.residual.interleaved(), &[0.0, 0.5, 0.0]);
        assert_ne!(
            products.source.produced_by.scope,
            products.residual.produced_by.scope
        );
    }
}
