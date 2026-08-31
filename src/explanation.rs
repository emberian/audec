//! Durable explanation definitions and their frozen render product.
//!
//! An explanation is a recipe plus evidence, not a correctness assertion.
//! Compilation resolves project and artifact references against one frozen
//! context. Rendering the resulting product cannot observe later edits.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use crate::arrangement;
use crate::artifact_catalog::ArtifactId;
use crate::aspect::{Aspect, ConcreteAspect, FrameSpan};
use crate::audio::{ProjectAudio, ProjectFrame};
use crate::daw_project::{ProjectDomain, ProjectRevisions};
use crate::daw_render::RenderCancellation;
use crate::ontology;
use crate::reconstruction::{ReconstructionEvidenceId, ReconstructionTrackId};
use crate::sequencer;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExplanationId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HpssComponentKind {
    Harmonic,
    Percussive,
}

/// Stable recipe references. Artifact-backed scopes always name the exact
/// analysis output; no scope reaches into lens-local mutable state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplanationScope {
    ArrangementClip(arrangement::ClipId),
    PatternClip(sequencer::PatternClipId),
    ReconstructionTrack {
        artifact: ArtifactId,
        track: ReconstructionTrackId,
    },
    LoomSketch {
        artifact: ArtifactId,
        clusters: Vec<usize>,
    },
    HpssComponent {
        artifact: ArtifactId,
        component: HpssComponentKind,
    },
    ModelClaim {
        artifact: ArtifactId,
        claim: u64,
    },
    /// Sum existing definitions. Order is canonicalized by ID before compile;
    /// duplicates are rejected rather than silently double-counted.
    Group(Vec<ExplanationId>),
}

impl ExplanationScope {
    pub fn normalize(&mut self) -> Result<(), ExplanationError> {
        match self {
            Self::LoomSketch { clusters, .. } => {
                clusters.sort_unstable();
                clusters.dedup();
                if clusters.is_empty() {
                    return Err(ExplanationError::EmptyScope);
                }
            }
            Self::Group(explanations) => {
                explanations.sort_unstable();
                let before = explanations.len();
                explanations.dedup();
                if explanations.is_empty() {
                    return Err(ExplanationError::EmptyScope);
                }
                if explanations.len() != before {
                    return Err(ExplanationError::DuplicateGroupMember);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn artifacts(&self) -> BTreeSet<ArtifactId> {
        match self {
            Self::ReconstructionTrack { artifact, .. }
            | Self::LoomSketch { artifact, .. }
            | Self::HpssComponent { artifact, .. }
            | Self::ModelClaim { artifact, .. } => BTreeSet::from([*artifact]),
            _ => BTreeSet::new(),
        }
    }

    /// Conservative project dependencies. The compiler may narrow this pin,
    /// but must never omit a domain that can change audible output.
    pub fn project_dependencies(&self) -> BTreeSet<ProjectDomain> {
        match self {
            Self::ArrangementClip(_) => BTreeSet::from([
                ProjectDomain::Arrangement,
                ProjectDomain::Automation,
                ProjectDomain::Assets,
                ProjectDomain::Mixer,
                ProjectDomain::Bindings,
            ]),
            Self::PatternClip(_) => BTreeSet::from([
                ProjectDomain::Sequencer,
                ProjectDomain::Automation,
                ProjectDomain::Assets,
                ProjectDomain::Mixer,
                ProjectDomain::Bindings,
            ]),
            Self::ReconstructionTrack { .. }
            | Self::LoomSketch { .. }
            | Self::HpssComponent { .. }
            | Self::ModelClaim { .. } => BTreeSet::new(),
            Self::Group(_) => BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExplanationEvidenceRef {
    Air(ontology::EvidenceId),
    Reconstruction {
        artifact: ArtifactId,
        evidence: ReconstructionEvidenceId,
    },
    Artifact(ArtifactId),
}

/// Persistent semantic recipe. Rendered PCM and cache state never live here.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplanationDefinition {
    pub id: ExplanationId,
    pub label: String,
    pub scope: ExplanationScope,
    /// Claimed extent; compilation resolves it to concrete geometry.
    pub extent: Aspect,
    pub evidence: Vec<ExplanationEvidenceRef>,
    pub provenance: ontology::Provenance,
}

impl ExplanationDefinition {
    pub fn normalize_and_validate(&mut self) -> Result<(), ExplanationError> {
        if self.id.0 == 0 {
            return Err(ExplanationError::ZeroIdentity);
        }
        if self.label.trim().is_empty() {
            return Err(ExplanationError::EmptyLabel);
        }
        self.scope.normalize()?;
        self.extent = crate::aspect::normalize(self.extent.clone());
        self.evidence.sort();
        self.evidence.dedup();
        Ok(())
    }
}

/// Exact dependency state captured by compilation. Artifact IDs are already
/// immutable content addresses; project dependencies retain only relevant
/// per-domain generations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExplanationDependencyPin {
    pub project: Vec<(ProjectDomain, u64)>,
    pub artifacts: Vec<ArtifactId>,
}

impl ExplanationDependencyPin {
    pub fn from_dependencies(
        revisions: ProjectRevisions,
        domains: impl IntoIterator<Item = ProjectDomain>,
        artifacts: impl IntoIterator<Item = ArtifactId>,
    ) -> Self {
        let mut project = domains
            .into_iter()
            .map(|domain| (domain, revisions.domain(domain)))
            .collect::<Vec<_>>();
        project.sort_unstable_by_key(|(domain, _)| *domain);
        project.dedup_by_key(|(domain, _)| *domain);
        let mut artifacts = artifacts.into_iter().collect::<Vec<_>>();
        artifacts.sort_unstable();
        artifacts.dedup();
        Self { project, artifacts }
    }

    pub fn is_stale(&self, revisions: ProjectRevisions) -> bool {
        self.project
            .iter()
            .any(|(domain, generation)| revisions.domain(*domain) != *generation)
    }
}

#[derive(Clone, Debug)]
pub struct RenderedExplanation {
    pub origin_frame: i64,
    pub audio: ProjectAudio,
}

pub trait FrozenExplanationRenderer: Send + Sync {
    fn render(
        &self,
        window: FrameSpan,
        cancellation: &RenderCancellation,
    ) -> Result<RenderedExplanation, ExplanationError>;
}

/// Common immutable product consumed by comparison, coverage, and audition.
#[derive(Clone)]
pub struct CompiledExplanation {
    definition: ExplanationDefinition,
    extent: ConcreteAspect,
    evidence: Arc<[ExplanationEvidenceRef]>,
    dependencies: ExplanationDependencyPin,
    renderer: Arc<dyn FrozenExplanationRenderer>,
}

impl fmt::Debug for CompiledExplanation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledExplanation")
            .field("definition", &self.definition)
            .field("extent", &self.extent)
            .field("evidence", &self.evidence)
            .field("dependencies", &self.dependencies)
            .finish_non_exhaustive()
    }
}

impl CompiledExplanation {
    pub fn new(
        mut definition: ExplanationDefinition,
        extent: ConcreteAspect,
        dependencies: ExplanationDependencyPin,
        renderer: Arc<dyn FrozenExplanationRenderer>,
    ) -> Result<Self, ExplanationError> {
        definition.normalize_and_validate()?;
        if extent.is_empty() {
            return Err(ExplanationError::EmptyExtent);
        }
        Ok(Self {
            evidence: definition.evidence.clone().into(),
            definition,
            extent,
            dependencies,
            renderer,
        })
    }

    pub fn definition(&self) -> &ExplanationDefinition {
        &self.definition
    }

    pub fn extent(&self) -> &ConcreteAspect {
        &self.extent
    }

    pub fn evidence(&self) -> &[ExplanationEvidenceRef] {
        &self.evidence
    }

    pub fn dependencies(&self) -> &ExplanationDependencyPin {
        &self.dependencies
    }

    pub fn render(
        &self,
        window: FrameSpan,
        cancellation: &RenderCancellation,
    ) -> Result<RenderedExplanation, ExplanationError> {
        if window.start >= window.end {
            return Err(ExplanationError::InvalidWindow(window));
        }
        self.renderer.render(window, cancellation)
    }
}

/// Integration seam implemented by the convergence layer. It may resolve DAW
/// solo schedules, artifact payloads, or groups, but always returns the same
/// frozen product.
pub trait ExplanationCompiler {
    fn compile(
        &self,
        definition: &ExplanationDefinition,
        cancellation: &RenderCancellation,
    ) -> Result<CompiledExplanation, ExplanationError>;
}

/// Useful frozen renderer for already-materialized artifact output and tests.
#[derive(Clone, Debug)]
pub struct PcmExplanationRenderer {
    pub origin_frame: i64,
    pub audio: ProjectAudio,
}

impl FrozenExplanationRenderer for PcmExplanationRenderer {
    fn render(
        &self,
        window: FrameSpan,
        cancellation: &RenderCancellation,
    ) -> Result<RenderedExplanation, ExplanationError> {
        if cancellation.is_cancelled() {
            return Err(ExplanationError::Cancelled);
        }
        let end = self
            .origin_frame
            .checked_add(self.audio.frame_count().0 as i64)
            .ok_or(ExplanationError::WindowOutsideCompiledExtent(window))?;
        if window.start < self.origin_frame || window.end > end || window.start >= window.end {
            return Err(ExplanationError::WindowOutsideCompiledExtent(window));
        }
        let channels = usize::from(self.audio.format().channels.get());
        let first = usize::try_from(window.start - self.origin_frame)
            .map_err(|_| ExplanationError::WindowOutsideCompiledExtent(window))?;
        let frames = usize::try_from(window.end - window.start)
            .map_err(|_| ExplanationError::WindowOutsideCompiledExtent(window))?;
        let sample_start = first
            .checked_mul(channels)
            .ok_or(ExplanationError::RenderTooLarge)?;
        let sample_end = first
            .checked_add(frames)
            .and_then(|end| end.checked_mul(channels))
            .ok_or(ExplanationError::RenderTooLarge)?;
        let audio = ProjectAudio::new(
            self.audio.format(),
            Arc::from(&self.audio.interleaved()[sample_start..sample_end]),
        )
        .map_err(|error| ExplanationError::Render(error.to_string()))?;
        debug_assert_eq!(audio.frame_count(), ProjectFrame(frames as u64));
        Ok(RenderedExplanation {
            origin_frame: window.start,
            audio,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplanationError {
    ZeroIdentity,
    EmptyLabel,
    EmptyScope,
    EmptyExtent,
    DuplicateGroupMember,
    MissingDefinition(ExplanationId),
    CyclicGroup(Vec<ExplanationId>),
    InvalidWindow(FrameSpan),
    WindowOutsideCompiledExtent(FrameSpan),
    RenderTooLarge,
    Cancelled,
    Unresolvable(String),
    Render(String),
}

impl fmt::Display for ExplanationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroIdentity => formatter.write_str("explanation identity must be non-zero"),
            Self::EmptyLabel => formatter.write_str("explanation label must not be empty"),
            Self::EmptyScope => formatter.write_str("explanation scope must not be empty"),
            Self::EmptyExtent => formatter.write_str("compiled explanation extent is empty"),
            Self::DuplicateGroupMember => {
                formatter.write_str("explanation group contains a duplicate member")
            }
            Self::MissingDefinition(id) => write!(formatter, "explanation {} is missing", id.0),
            Self::CyclicGroup(path) => write!(formatter, "cyclic explanation group: {path:?}"),
            Self::InvalidWindow(window) => write!(formatter, "invalid render window {window:?}"),
            Self::WindowOutsideCompiledExtent(window) => {
                write!(
                    formatter,
                    "window {window:?} is outside the compiled signal"
                )
            }
            Self::RenderTooLarge => formatter.write_str("explanation render is too large"),
            Self::Cancelled => formatter.write_str("explanation render cancelled"),
            Self::Unresolvable(message) => write!(formatter, "unresolvable explanation: {message}"),
            Self::Render(message) => write!(formatter, "explanation render failed: {message}"),
        }
    }
}

impl std::error::Error for ExplanationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{BandSpan, ChannelMask, ConcreteRegion, SignalLayer};
    use crate::audio::AudioFormat;
    use crate::ontology::Producer;

    fn provenance() -> ontology::Provenance {
        ontology::Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    #[test]
    fn pcm_renderer_returns_the_exact_requested_aligned_window() {
        let format = AudioFormat::new(48_000, 1).unwrap();
        let renderer = Arc::new(PcmExplanationRenderer {
            origin_frame: 10,
            audio: ProjectAudio::from_interleaved(format, vec![1.0, 2.0, 3.0, 4.0]).unwrap(),
        });
        let definition = ExplanationDefinition {
            id: ExplanationId(1),
            label: "artifact".into(),
            scope: ExplanationScope::ModelClaim {
                artifact: ArtifactId(crate::artifact_catalog::ContentDigest::new(
                    crate::artifact_catalog::DigestAlgorithm::Sha256,
                    [1; 32],
                )),
                claim: 2,
            },
            extent: Aspect::Time(FrameSpan { start: 10, end: 14 }),
            evidence: Vec::new(),
            provenance: provenance(),
        };
        let extent = ConcreteAspect::new(
            vec![ConcreteRegion {
                time: FrameSpan { start: 10, end: 14 },
                band: BandSpan {
                    min_hz: 0.0,
                    max_hz: 24_000.0,
                },
                channels: ChannelMask(1),
            }],
            SignalLayer::Explanation(crate::aspect::ExplanationRef::Definition(1)),
        )
        .unwrap();
        let compiled = CompiledExplanation::new(
            definition,
            extent,
            ExplanationDependencyPin::default(),
            renderer,
        )
        .unwrap();
        let rendered = compiled
            .render(FrameSpan { start: 11, end: 13 }, &RenderCancellation::new())
            .unwrap();
        assert_eq!(rendered.origin_frame, 11);
        assert_eq!(rendered.audio.interleaved(), &[2.0, 3.0]);
    }
}
