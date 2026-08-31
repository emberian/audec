//! Dispatch and composition for frozen explanation scopes.
//!
//! This module is the anti-corruption seam between semantic explanation
//! definitions and mutable DAW/analysis domains. Resolvers must freeze every
//! input before returning. The compiler then performs deterministic group
//! composition without teaching the interpretation domain about render
//! schedules, Loom internals, HPSS masks, or worker protocols.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::arrangement;
use crate::artifact_catalog::{ArtifactCatalog, ArtifactCatalogError, ArtifactId, ArtifactKind};
use crate::aspect::{ConcreteAspect, ExplanationRef, FrameSpan, SignalLayer};
use crate::audio::ProjectAudio;
use crate::daw_project::{ProjectDomain, ProjectRevisions};
use crate::daw_render::RenderCancellation;
use crate::explanation::{
    CompiledExplanation, ExplanationCompiler, ExplanationDefinition, ExplanationDependencyPin,
    ExplanationError, ExplanationEvidenceRef, ExplanationId, ExplanationScope,
    FrozenExplanationRenderer, HpssComponentKind, RenderedExplanation,
};
use crate::interpretation::InterpretationStore;
use crate::reconstruction::ReconstructionTrackId;
use crate::sequencer;

/// A fully frozen leaf returned by a domain adapter.
#[derive(Clone)]
pub struct FrozenScope {
    pub extent: ConcreteAspect,
    pub evidence: Vec<ExplanationEvidenceRef>,
    pub project_dependencies: BTreeSet<ProjectDomain>,
    pub artifacts: BTreeSet<ArtifactId>,
    pub renderer: Arc<dyn FrozenExplanationRenderer>,
}

impl FrozenScope {
    pub fn new(
        extent: ConcreteAspect,
        evidence: Vec<ExplanationEvidenceRef>,
        renderer: Arc<dyn FrozenExplanationRenderer>,
    ) -> Result<Self, ExplanationError> {
        if extent.is_empty() {
            return Err(ExplanationError::EmptyExtent);
        }
        let mut evidence = evidence;
        evidence.sort();
        evidence.dedup();
        Ok(Self {
            extent,
            evidence,
            project_dependencies: BTreeSet::new(),
            artifacts: BTreeSet::new(),
            renderer,
        })
    }

    pub fn with_project_dependencies(
        mut self,
        domains: impl IntoIterator<Item = ProjectDomain>,
    ) -> Self {
        self.project_dependencies.extend(domains);
        self
    }

    pub fn with_artifacts(mut self, artifacts: impl IntoIterator<Item = ArtifactId>) -> Self {
        self.artifacts.extend(artifacts);
        self
    }
}

/// Mutable project state is allowed only behind this freezing boundary.
pub trait DawScopeResolver: Send + Sync {
    fn arrangement_clip(
        &self,
        clip: arrangement::ClipId,
        cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError>;

    fn pattern_clip(
        &self,
        clip: sequencer::PatternClipId,
        cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError>;
}

/// Artifact-backed analyzers keep their differing semantics behind explicit
/// arms. An anonymous reconstruction track is never silently treated as a
/// Loom family, HPSS stem, or model identity.
pub trait AnalysisScopeResolver: Send + Sync {
    fn reconstruction_track(
        &self,
        artifact: ArtifactId,
        track: ReconstructionTrackId,
        cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError>;

    fn loom_clusters(
        &self,
        artifact: ArtifactId,
        clusters: &[usize],
        cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError>;

    fn hpss_component(
        &self,
        artifact: ArtifactId,
        component: HpssComponentKind,
        cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError>;

    fn model_claim(
        &self,
        artifact: ArtifactId,
        claim: u64,
        cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError>;
}

pub trait ExplanationDefinitions: Send + Sync {
    fn explanation(&self, id: ExplanationId) -> Option<ExplanationDefinition>;
}

impl ExplanationDefinitions for InterpretationStore {
    fn explanation(&self, id: ExplanationId) -> Option<ExplanationDefinition> {
        self.explanation(id).cloned()
    }
}

/// One keyed signal inside an immutable analysis artifact. Workers and local
/// analyzers may populate this neutral payload without exposing their model
/// types to the explanation compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactScopeKey {
    ReconstructionTrack(ReconstructionTrackId),
    LoomCluster(usize),
    HpssComponent(HpssComponentKind),
    ModelClaim(u64),
}

#[derive(Clone, Default)]
pub struct ArtifactExplanationPayload {
    pub signals: BTreeMap<ArtifactScopeKey, FrozenScope>,
}

/// Executable resolver for content-addressed analysis payloads. Its catalog
/// lookup verifies the exact artifact identity; semantic kind checks prevent
/// accidentally interpreting one analyzer's payload as another's.
pub struct CatalogAnalysisResolver<'a> {
    pub catalog: &'a ArtifactCatalog,
}

impl CatalogAnalysisResolver<'_> {
    fn payload(
        &self,
        artifact: ArtifactId,
        expected_kind: ArtifactKind,
    ) -> Result<Arc<ArtifactExplanationPayload>, ExplanationError> {
        let descriptor = self.catalog.descriptor(artifact).ok_or_else(|| {
            ExplanationError::Unresolvable(format!("analysis artifact {artifact:?} is missing"))
        })?;
        if descriptor.kind != expected_kind {
            return Err(ExplanationError::Unresolvable(format!(
                "artifact {artifact:?} has kind {:?}, expected {expected_kind:?}",
                descriptor.kind
            )));
        }
        self.catalog
            .get::<ArtifactExplanationPayload>(artifact)
            .map_err(catalog_error)
    }

    fn one(
        &self,
        artifact: ArtifactId,
        kind: ArtifactKind,
        key: ArtifactScopeKey,
    ) -> Result<FrozenScope, ExplanationError> {
        let payload = self.payload(artifact, kind)?;
        let mut scope = payload.signals.get(&key).cloned().ok_or_else(|| {
            ExplanationError::Unresolvable(format!(
                "artifact {artifact:?} has no explanation signal {key:?}"
            ))
        })?;
        scope.artifacts.insert(artifact);
        Ok(scope)
    }
}

impl AnalysisScopeResolver for CatalogAnalysisResolver<'_> {
    fn reconstruction_track(
        &self,
        artifact: ArtifactId,
        track: ReconstructionTrackId,
        _cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError> {
        self.one(
            artifact,
            ArtifactKind::ReconstructionSet,
            ArtifactScopeKey::ReconstructionTrack(track),
        )
    }

    fn loom_clusters(
        &self,
        artifact: ArtifactId,
        clusters: &[usize],
        cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError> {
        if cancellation.is_cancelled() {
            return Err(ExplanationError::Cancelled);
        }
        if clusters.is_empty() {
            return Err(ExplanationError::EmptyScope);
        }
        let payload = self.payload(artifact, ArtifactKind::LoomSketch)?;
        let scopes = clusters
            .iter()
            .map(|cluster| {
                payload
                    .signals
                    .get(&ArtifactScopeKey::LoomCluster(*cluster))
                    .cloned()
                    .ok_or_else(|| {
                        ExplanationError::Unresolvable(format!(
                            "Loom artifact {artifact:?} has no cluster {cluster}"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut combined = combine_frozen_scopes(scopes)?;
        combined.artifacts.insert(artifact);
        Ok(combined)
    }

    fn hpss_component(
        &self,
        artifact: ArtifactId,
        component: HpssComponentKind,
        _cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError> {
        self.one(
            artifact,
            ArtifactKind::Hpss,
            ArtifactScopeKey::HpssComponent(component),
        )
    }

    fn model_claim(
        &self,
        artifact: ArtifactId,
        claim: u64,
        _cancellation: &RenderCancellation,
    ) -> Result<FrozenScope, ExplanationError> {
        self.one(
            artifact,
            ArtifactKind::ModelClaim,
            ArtifactScopeKey::ModelClaim(claim),
        )
    }
}

fn catalog_error(error: ArtifactCatalogError) -> ExplanationError {
    ExplanationError::Unresolvable(error.to_string())
}

/// Deterministic dispatcher and group compiler.
pub struct ResolvingExplanationCompiler<'a> {
    pub revisions: ProjectRevisions,
    pub definitions: &'a dyn ExplanationDefinitions,
    pub daw: &'a dyn DawScopeResolver,
    pub analysis: &'a dyn AnalysisScopeResolver,
}

impl ExplanationCompiler for ResolvingExplanationCompiler<'_> {
    fn compile(
        &self,
        definition: &ExplanationDefinition,
        cancellation: &RenderCancellation,
    ) -> Result<CompiledExplanation, ExplanationError> {
        let mut stack = Vec::new();
        self.compile_inner(definition, cancellation, &mut stack)
    }
}

impl ResolvingExplanationCompiler<'_> {
    fn compile_inner(
        &self,
        definition: &ExplanationDefinition,
        cancellation: &RenderCancellation,
        stack: &mut Vec<ExplanationId>,
    ) -> Result<CompiledExplanation, ExplanationError> {
        if cancellation.is_cancelled() {
            return Err(ExplanationError::Cancelled);
        }
        if let Some(index) = stack.iter().position(|id| *id == definition.id) {
            let mut cycle = stack[index..].to_vec();
            cycle.push(definition.id);
            return Err(ExplanationError::CyclicGroup(cycle));
        }
        stack.push(definition.id);
        let result = match &definition.scope {
            ExplanationScope::ArrangementClip(clip) => {
                self.daw.arrangement_clip(*clip, cancellation)
            }
            ExplanationScope::PatternClip(clip) => self.daw.pattern_clip(*clip, cancellation),
            ExplanationScope::ReconstructionTrack { artifact, track } => self
                .analysis
                .reconstruction_track(*artifact, *track, cancellation),
            ExplanationScope::LoomSketch { artifact, clusters } => {
                self.analysis
                    .loom_clusters(*artifact, clusters, cancellation)
            }
            ExplanationScope::HpssComponent {
                artifact,
                component,
            } => self
                .analysis
                .hpss_component(*artifact, *component, cancellation),
            ExplanationScope::ModelClaim { artifact, claim } => {
                self.analysis.model_claim(*artifact, *claim, cancellation)
            }
            ExplanationScope::Group(members) => {
                let mut children = Vec::with_capacity(members.len());
                for id in members {
                    let child = self
                        .definitions
                        .explanation(*id)
                        .ok_or(ExplanationError::MissingDefinition(*id))?;
                    children.push(self.compile_inner(&child, cancellation, stack)?);
                }
                combine_compiled(children)
            }
        };
        stack.pop();
        let mut scope = result?;
        scope
            .project_dependencies
            .extend(definition.scope.project_dependencies());
        scope.artifacts.extend(definition.scope.artifacts());
        scope.evidence.extend(definition.evidence.iter().cloned());
        scope.evidence.sort();
        scope.evidence.dedup();
        scope.extent.signal = SignalLayer::Explanation(ExplanationRef::Definition(definition.id.0));
        let dependencies = ExplanationDependencyPin::from_dependencies(
            self.revisions,
            scope.project_dependencies,
            scope.artifacts,
        );
        CompiledExplanation::new(
            definition.clone(),
            scope.extent,
            dependencies,
            scope.renderer,
        )
    }
}

fn combine_compiled(children: Vec<CompiledExplanation>) -> Result<FrozenScope, ExplanationError> {
    if children.is_empty() {
        return Err(ExplanationError::EmptyScope);
    }
    let mut regions = Vec::new();
    let mut objects = Vec::new();
    let mut evidence = Vec::new();
    let mut project_dependencies = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    for child in &children {
        regions.extend(child.extent().regions.iter().copied());
        objects.extend(child.extent().objects.iter().copied());
        evidence.extend(child.evidence().iter().cloned());
        project_dependencies.extend(
            child
                .dependencies()
                .project
                .iter()
                .map(|(domain, _)| *domain),
        );
        artifacts.extend(child.dependencies().artifacts.iter().copied());
    }
    let mut extent = ConcreteAspect::new(regions, SignalLayer::Source)
        .map_err(|error| ExplanationError::Unresolvable(error.to_string()))?;
    objects.sort();
    objects.dedup();
    extent.objects = objects;
    evidence.sort();
    evidence.dedup();
    Ok(FrozenScope {
        extent,
        evidence,
        project_dependencies,
        artifacts,
        renderer: Arc::new(SumRenderer { children }),
    })
}

fn combine_frozen_scopes(scopes: Vec<FrozenScope>) -> Result<FrozenScope, ExplanationError> {
    if scopes.is_empty() {
        return Err(ExplanationError::EmptyScope);
    }
    let mut regions = Vec::new();
    let mut objects = Vec::new();
    let mut evidence = Vec::new();
    let mut project_dependencies = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    let mut renderers = Vec::new();
    for scope in scopes {
        regions.extend(scope.extent.regions);
        objects.extend(scope.extent.objects);
        evidence.extend(scope.evidence);
        project_dependencies.extend(scope.project_dependencies);
        artifacts.extend(scope.artifacts);
        renderers.push(scope.renderer);
    }
    let mut extent = ConcreteAspect::new(regions, SignalLayer::Source)
        .map_err(|error| ExplanationError::Unresolvable(error.to_string()))?;
    objects.sort();
    objects.dedup();
    extent.objects = objects;
    evidence.sort();
    evidence.dedup();
    Ok(FrozenScope {
        extent,
        evidence,
        project_dependencies,
        artifacts,
        renderer: Arc::new(RendererSum { renderers }),
    })
}

struct SumRenderer {
    children: Vec<CompiledExplanation>,
}

impl FrozenExplanationRenderer for SumRenderer {
    fn render(
        &self,
        window: FrameSpan,
        cancellation: &RenderCancellation,
    ) -> Result<RenderedExplanation, ExplanationError> {
        let renders = self
            .children
            .iter()
            .map(|child| child.render(window, cancellation))
            .collect::<Result<Vec<_>, _>>()?;
        sum_renders(renders)
    }
}

struct RendererSum {
    renderers: Vec<Arc<dyn FrozenExplanationRenderer>>,
}

impl FrozenExplanationRenderer for RendererSum {
    fn render(
        &self,
        window: FrameSpan,
        cancellation: &RenderCancellation,
    ) -> Result<RenderedExplanation, ExplanationError> {
        let renders = self
            .renderers
            .iter()
            .map(|renderer| renderer.render(window, cancellation))
            .collect::<Result<Vec<_>, _>>()?;
        sum_renders(renders)
    }
}

fn sum_renders(renders: Vec<RenderedExplanation>) -> Result<RenderedExplanation, ExplanationError> {
    let mut renders = renders.into_iter();
    let first = renders.next().ok_or(ExplanationError::EmptyScope)?;
    let origin = first.origin_frame;
    let format = first.audio.format();
    let frames = first.audio.frame_count();
    let mut samples = first
        .audio
        .interleaved()
        .iter()
        .map(|sample| if sample.is_finite() { *sample } else { 0.0 })
        .collect::<Vec<_>>();
    for render in renders {
        if render.origin_frame != origin
            || render.audio.format() != format
            || render.audio.frame_count() != frames
        {
            return Err(ExplanationError::Render(
                "group members returned unaligned audio".into(),
            ));
        }
        for (sum, sample) in samples.iter_mut().zip(render.audio.interleaved()) {
            let sample = if sample.is_finite() { *sample } else { 0.0 };
            let next = *sum + sample;
            *sum = if next.is_finite() { next } else { 0.0 };
        }
    }
    let audio = ProjectAudio::from_interleaved(format, samples)
        .map_err(|error| ExplanationError::Render(error.to_string()))?;
    Ok(RenderedExplanation {
        origin_frame: origin,
        audio,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{BandSpan, ChannelMask, ConcreteRegion};
    use crate::audio::AudioFormat;
    use crate::explanation::PcmExplanationRenderer;
    use crate::ontology::{Producer, Provenance};

    fn frozen(value: f32) -> FrozenScope {
        let extent = ConcreteAspect::new(
            vec![ConcreteRegion {
                time: FrameSpan { start: 0, end: 2 },
                band: BandSpan::new(0.0, 24_000.0).unwrap(),
                channels: ChannelMask(1),
            }],
            SignalLayer::Source,
        )
        .unwrap();
        FrozenScope::new(
            extent,
            Vec::new(),
            Arc::new(PcmExplanationRenderer {
                origin_frame: 0,
                audio: ProjectAudio::from_interleaved(
                    AudioFormat::new(48_000, 1).unwrap(),
                    vec![value; 2],
                )
                .unwrap(),
            }),
        )
        .unwrap()
    }

    struct Daw;
    impl DawScopeResolver for Daw {
        fn arrangement_clip(
            &self,
            clip: arrangement::ClipId,
            _: &RenderCancellation,
        ) -> Result<FrozenScope, ExplanationError> {
            Ok(frozen(clip.get() as f32))
        }
        fn pattern_clip(
            &self,
            clip: sequencer::PatternClipId,
            _: &RenderCancellation,
        ) -> Result<FrozenScope, ExplanationError> {
            Ok(frozen(clip.get() as f32))
        }
    }

    struct Analysis;
    impl AnalysisScopeResolver for Analysis {
        fn reconstruction_track(
            &self,
            _: ArtifactId,
            _: ReconstructionTrackId,
            _: &RenderCancellation,
        ) -> Result<FrozenScope, ExplanationError> {
            Err(ExplanationError::Unresolvable("unused".into()))
        }
        fn loom_clusters(
            &self,
            _: ArtifactId,
            _: &[usize],
            _: &RenderCancellation,
        ) -> Result<FrozenScope, ExplanationError> {
            Err(ExplanationError::Unresolvable("unused".into()))
        }
        fn hpss_component(
            &self,
            _: ArtifactId,
            _: HpssComponentKind,
            _: &RenderCancellation,
        ) -> Result<FrozenScope, ExplanationError> {
            Err(ExplanationError::Unresolvable("unused".into()))
        }
        fn model_claim(
            &self,
            _: ArtifactId,
            _: u64,
            _: &RenderCancellation,
        ) -> Result<FrozenScope, ExplanationError> {
            Err(ExplanationError::Unresolvable("unused".into()))
        }
    }

    struct Definitions(BTreeMap<ExplanationId, ExplanationDefinition>);
    impl ExplanationDefinitions for Definitions {
        fn explanation(&self, id: ExplanationId) -> Option<ExplanationDefinition> {
            self.0.get(&id).cloned()
        }
    }

    fn definition(id: u64, scope: ExplanationScope) -> ExplanationDefinition {
        ExplanationDefinition {
            id: ExplanationId(id),
            label: format!("e{id}"),
            scope,
            extent: crate::aspect::Aspect::Time(FrameSpan { start: 0, end: 2 }),
            evidence: Vec::new(),
            provenance: Provenance {
                producer: Producer::Human { name: None },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
        }
    }

    #[test]
    fn group_dispatch_sums_frozen_children_and_relabels_extent() {
        let first = definition(
            1,
            ExplanationScope::ArrangementClip(arrangement::ClipId::from_raw(1)),
        );
        let second = definition(
            2,
            ExplanationScope::PatternClip(sequencer::PatternClipId::from_raw(2)),
        );
        let group = definition(
            3,
            ExplanationScope::Group(vec![ExplanationId(1), ExplanationId(2)]),
        );
        let definitions = Definitions(BTreeMap::from([(first.id, first), (second.id, second)]));
        let compiler = ResolvingExplanationCompiler {
            revisions: ProjectRevisions::default(),
            definitions: &definitions,
            daw: &Daw,
            analysis: &Analysis,
        };
        let compiled = compiler
            .compile(&group, &RenderCancellation::new())
            .unwrap();
        assert_eq!(
            compiled.extent().signal,
            SignalLayer::Explanation(ExplanationRef::Definition(3))
        );
        assert_eq!(
            compiled
                .render(FrameSpan { start: 0, end: 2 }, &RenderCancellation::new())
                .unwrap()
                .audio
                .interleaved(),
            &[3.0, 3.0]
        );
    }
}
