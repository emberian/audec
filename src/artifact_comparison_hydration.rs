//! Exact analysis-artifact hydration for comparison explanations.
//!
//! Analysis producers retain their native result types. This boundary turns
//! their real rendered PCM into the neutral `ArtifactExplanationPayload`
//! consumed by `CatalogAnalysisResolver`, while pinning the artifact source,
//! recipe, project revision, publication, and catalog generation. No decoder,
//! resampler, source fallback, or synthetic silence exists here.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::{
    ArtifactCatalog, ArtifactCatalogError, ArtifactDescriptor, ArtifactId, ArtifactKind,
    ContentDigest,
};
use crate::aspect::{
    BandSpan, ChannelMask, ConcreteAspect, ConcreteRegion, FrameSpan, SignalLayer,
};
use crate::audio::{AudioFormat, ProjectAudio};
use crate::comparison::{ComparisonId, ExactRenderDigest};
use crate::comparison_runtime::exact_audio_digest;
use crate::comparison_runtime::executor::ComparisonSemanticSnapshot;
use crate::daw_project::ProjectRevisions;
use crate::daw_render::RenderCancellation;
use crate::explanation::{
    ExplanationEvidenceRef, ExplanationId, ExplanationScope, HpssComponentKind,
    PcmExplanationRenderer, RenderedExplanation,
};
use crate::explanation_adapters::{ArtifactExplanationPayload, ArtifactScopeKey, FrozenScope};
use crate::hpss::HpssResult;
use crate::interpretation::InterpretationStore;
use crate::loom::SequenceSketch;
use crate::reconstruction::ReconstructionTrackId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactComparisonPin {
    pub artifact: ArtifactId,
    pub source_digest: ContentDigest,
    pub recipe_digest: ContentDigest,
    pub project_revisions: ProjectRevisions,
    pub publication_generation: u64,
    pub catalog_generation: u64,
}

impl ArtifactComparisonPin {
    pub fn from_descriptor(
        descriptor: &ArtifactDescriptor,
        project_revisions: ProjectRevisions,
        publication_generation: u64,
        catalog_generation: u64,
    ) -> Self {
        Self {
            artifact: descriptor.id,
            source_digest: descriptor.source_digest,
            recipe_digest: descriptor.recipe_digest,
            project_revisions,
            publication_generation,
            catalog_generation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactHydrationContext {
    pub project_revisions: ProjectRevisions,
    pub publication_generation: u64,
    pub catalog_generation: u64,
}

#[derive(Clone, Debug)]
pub struct ArtifactComparisonSignal {
    pub key: ArtifactScopeKey,
    pub origin_frame: i64,
    pub audio: ProjectAudio,
    pub digest: ExactRenderDigest,
}

impl ArtifactComparisonSignal {
    pub fn new(
        key: ArtifactScopeKey,
        origin_frame: i64,
        audio: ProjectAudio,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        let digest = exact_audio_digest(&audio, origin_frame)
            .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))?;
        Ok(Self {
            key,
            origin_frame,
            audio,
            digest,
        })
    }

    fn end_frame(&self) -> Result<i64, ArtifactComparisonHydrationError> {
        let frames = i64::try_from(self.audio.frame_count().0)
            .map_err(|_| ArtifactComparisonHydrationError::SignalTooLarge)?;
        self.origin_frame
            .checked_add(frames)
            .ok_or(ArtifactComparisonHydrationError::SignalTooLarge)
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactComparisonPayload {
    pub pin: ArtifactComparisonPin,
    signals: BTreeMap<ArtifactScopeKey, ArtifactComparisonSignal>,
}

impl ArtifactComparisonPayload {
    pub fn new(
        descriptor: &ArtifactDescriptor,
        pin: ArtifactComparisonPin,
        signals: impl IntoIterator<Item = ArtifactComparisonSignal>,
        cancellation: &RenderCancellation,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        validate_pin(descriptor, pin)?;
        if cancellation.is_cancelled() {
            return Err(ArtifactComparisonHydrationError::Cancelled);
        }
        let mut indexed = BTreeMap::new();
        for signal in signals {
            if cancellation.is_cancelled() {
                return Err(ArtifactComparisonHydrationError::Cancelled);
            }
            validate_signal(descriptor, &signal)?;
            let key = signal.key;
            if indexed.insert(key, signal).is_some() {
                return Err(ArtifactComparisonHydrationError::DuplicateSignal(key));
            }
        }
        if indexed.is_empty() {
            return Err(ArtifactComparisonHydrationError::EmptyPayload);
        }
        Ok(Self {
            pin,
            signals: indexed,
        })
    }

    pub fn signal(&self, key: ArtifactScopeKey) -> Option<&ArtifactComparisonSignal> {
        self.signals.get(&key)
    }

    pub fn signals(
        &self,
    ) -> impl ExactSizeIterator<Item = (&ArtifactScopeKey, &ArtifactComparisonSignal)> {
        self.signals.iter()
    }

    pub fn from_hpss(
        descriptor: &ArtifactDescriptor,
        pin: ArtifactComparisonPin,
        result: &HpssResult,
        cancellation: &RenderCancellation,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        require_kind(descriptor, ArtifactKind::Hpss)?;
        require_mono(descriptor)?;
        let format = AudioFormat::new(descriptor.sample_rate, 1)
            .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))?;
        let harmonic = ProjectAudio::from_interleaved(format, result.harmonic.clone())
            .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))?;
        let percussive = ProjectAudio::from_interleaved(format, result.percussive.clone())
            .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))?;
        Self::new(
            descriptor,
            pin,
            [
                ArtifactComparisonSignal::new(
                    ArtifactScopeKey::HpssComponent(HpssComponentKind::Harmonic),
                    descriptor.extent.start,
                    harmonic,
                )?,
                ArtifactComparisonSignal::new(
                    ArtifactScopeKey::HpssComponent(HpssComponentKind::Percussive),
                    descriptor.extent.start,
                    percussive,
                )?,
            ],
            cancellation,
        )
    }

    /// Render every Loom family independently from its retained phase-aware
    /// template/event data. `source_start_frame` is explicit because project
    /// placement and source coordinates are different domains.
    pub fn from_loom(
        descriptor: &ArtifactDescriptor,
        pin: ArtifactComparisonPin,
        sketch: &SequenceSketch,
        source_start_frame: u64,
        cancellation: &RenderCancellation,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        require_kind(descriptor, ArtifactKind::LoomSketch)?;
        require_mono(descriptor)?;
        if sketch.sample_rate != descriptor.sample_rate {
            return Err(ArtifactComparisonHydrationError::SampleRateMismatch {
                descriptor: descriptor.sample_rate,
                signal: sketch.sample_rate,
            });
        }
        let start = usize::try_from(source_start_frame)
            .map_err(|_| ArtifactComparisonHydrationError::SignalTooLarge)?;
        let frames = usize::try_from(
            i128::from(descriptor.extent.end) - i128::from(descriptor.extent.start),
        )
        .map_err(|_| ArtifactComparisonHydrationError::SignalTooLarge)?;
        let format = AudioFormat::new(descriptor.sample_rate, 1)
            .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))?;
        let mut signals = Vec::with_capacity(sketch.clusters.len());
        for target in &sketch.clusters {
            if cancellation.is_cancelled() {
                return Err(ArtifactComparisonHydrationError::Cancelled);
            }
            let mut isolated = sketch.clone();
            for cluster in &mut isolated.clusters {
                cluster.enabled = cluster.template.cluster_id == target.template.cluster_id;
            }
            let audio = ProjectAudio::from_interleaved(format, isolated.render_span(start, frames))
                .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))?;
            signals.push(ArtifactComparisonSignal::new(
                ArtifactScopeKey::LoomCluster(target.template.cluster_id),
                descriptor.extent.start,
                audio,
            )?);
        }
        Self::new(descriptor, pin, signals, cancellation)
    }

    /// Adopt an exact construction returned by the rhythm backend. The
    /// artifact-qualified claim key is the stable `PatternExplanation::claim_id`.
    pub fn from_rhythm_render(
        descriptor: &ArtifactDescriptor,
        pin: ArtifactComparisonPin,
        claim: u64,
        rendered: RenderedExplanation,
        cancellation: &RenderCancellation,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        Self::from_claim_render(descriptor, pin, claim, rendered, cancellation)
    }

    /// Adopt decoded model output only after the model adapter has returned
    /// exact project-format PCM. This boundary does not read model artifact
    /// paths or guess channel/sample-rate conversions.
    pub fn from_model_render(
        descriptor: &ArtifactDescriptor,
        pin: ArtifactComparisonPin,
        claim: u64,
        rendered: RenderedExplanation,
        cancellation: &RenderCancellation,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        Self::from_claim_render(descriptor, pin, claim, rendered, cancellation)
    }

    fn from_claim_render(
        descriptor: &ArtifactDescriptor,
        pin: ArtifactComparisonPin,
        claim: u64,
        rendered: RenderedExplanation,
        cancellation: &RenderCancellation,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        require_kind(descriptor, ArtifactKind::ModelClaim)?;
        Self::new(
            descriptor,
            pin,
            [ArtifactComparisonSignal::new(
                ArtifactScopeKey::ModelClaim(claim),
                rendered.origin_frame,
                rendered.audio,
            )?],
            cancellation,
        )
    }

    pub fn from_reconstruction_render(
        descriptor: &ArtifactDescriptor,
        pin: ArtifactComparisonPin,
        track: ReconstructionTrackId,
        rendered: RenderedExplanation,
        cancellation: &RenderCancellation,
    ) -> Result<Self, ArtifactComparisonHydrationError> {
        require_kind(descriptor, ArtifactKind::ReconstructionSet)?;
        Self::new(
            descriptor,
            pin,
            [ArtifactComparisonSignal::new(
                ArtifactScopeKey::ReconstructionTrack(track),
                rendered.origin_frame,
                rendered.audio,
            )?],
            cancellation,
        )
    }

    fn equivalent(&self, other: &Self) -> bool {
        self.pin == other.pin
            && self.signals.len() == other.signals.len()
            && self.signals.iter().all(|(key, left)| {
                other.signals.get(key).is_some_and(|right| {
                    left.origin_frame == right.origin_frame
                        && left.audio.format() == right.audio.format()
                        && left.audio.frame_count() == right.audio.frame_count()
                        && left.digest == right.digest
                })
            })
    }
}

/// Insert a producer-hydrated signal bank. Idempotence requires both the exact
/// descriptor and every aligned signal digest to agree.
pub fn insert_artifact_comparison_payload(
    catalog: &mut ArtifactCatalog,
    descriptor: ArtifactDescriptor,
    payload: Arc<ArtifactComparisonPayload>,
) -> Result<(), ArtifactComparisonHydrationError> {
    validate_pin(&descriptor, payload.pin)?;
    if let Some(existing_descriptor) = catalog.descriptor(descriptor.id) {
        if existing_descriptor != &descriptor {
            return Err(ArtifactComparisonHydrationError::DescriptorConflict(
                descriptor.id,
            ));
        }
        let existing = catalog
            .get::<ArtifactComparisonPayload>(descriptor.id)
            .map_err(|_| ArtifactComparisonHydrationError::PayloadTypeMismatch(descriptor.id))?;
        return if existing.equivalent(&payload) {
            Ok(())
        } else {
            Err(ArtifactComparisonHydrationError::SignalConflict(
                descriptor.id,
            ))
        };
    }
    catalog
        .insert(descriptor, payload)
        .map_err(ArtifactComparisonHydrationError::Catalog)
}

/// Resolve every artifact scope reachable from one comparison and build the
/// exact semantic snapshot expected by `ComparisonProductExecutor::capture`.
/// DAW-backed scopes pass through without creating catalog entries.
pub fn hydrate_comparison_semantics(
    source_catalog: &ArtifactCatalog,
    interpretations: Arc<InterpretationStore>,
    comparison: ComparisonId,
    context: ArtifactHydrationContext,
    cancellation: &RenderCancellation,
) -> Result<ComparisonSemanticSnapshot, ArtifactComparisonHydrationError> {
    if cancellation.is_cancelled() {
        return Err(ArtifactComparisonHydrationError::Cancelled);
    }
    let definition = interpretations.comparison(comparison).ok_or(
        ArtifactComparisonHydrationError::MissingComparison(comparison),
    )?;
    let mut requirements = BTreeMap::<ArtifactId, ArtifactRequirement>::new();
    let mut visiting = BTreeSet::new();
    collect_scope_requirements(
        definition.explanation,
        &interpretations,
        &mut visiting,
        &mut requirements,
    )?;
    let mut hydrated = ArtifactCatalog::new();
    for (artifact, requirement) in requirements {
        if cancellation.is_cancelled() {
            return Err(ArtifactComparisonHydrationError::Cancelled);
        }
        let descriptor = source_catalog
            .descriptor(artifact)
            .cloned()
            .ok_or(ArtifactComparisonHydrationError::MissingArtifact(artifact))?;
        require_kind(&descriptor, requirement.kind.clone())?;
        let payload = source_catalog
            .get::<ArtifactComparisonPayload>(artifact)
            .map_err(|error| match error {
                ArtifactCatalogError::Missing(_) => {
                    ArtifactComparisonHydrationError::MissingArtifact(artifact)
                }
                _ => ArtifactComparisonHydrationError::PayloadTypeMismatch(artifact),
            })?;
        validate_context(&descriptor, &payload, context)?;
        let mut explanation = ArtifactExplanationPayload::default();
        for key in requirement.keys {
            if cancellation.is_cancelled() {
                return Err(ArtifactComparisonHydrationError::Cancelled);
            }
            let signal = payload
                .signal(key)
                .ok_or(ArtifactComparisonHydrationError::MissingSignal { artifact, key })?;
            validate_signal(&descriptor, signal)?;
            let actual = exact_audio_digest(&signal.audio, signal.origin_frame)
                .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))?;
            if actual != signal.digest {
                return Err(ArtifactComparisonHydrationError::SignalDigestMismatch {
                    artifact,
                    key,
                });
            }
            explanation.signals.insert(
                key,
                FrozenScope::new(
                    signal_extent(&descriptor)?,
                    vec![ExplanationEvidenceRef::Artifact(artifact)],
                    Arc::new(PcmExplanationRenderer {
                        origin_frame: signal.origin_frame,
                        audio: signal.audio.clone(),
                    }),
                )?
                .with_artifacts([artifact]),
            );
        }
        hydrated
            .insert(descriptor, Arc::new(explanation))
            .map_err(ArtifactComparisonHydrationError::Catalog)?;
    }
    Ok(ComparisonSemanticSnapshot {
        interpretations,
        artifacts: Arc::new(hydrated),
    })
}

#[derive(Clone, Debug)]
struct ArtifactRequirement {
    kind: ArtifactKind,
    keys: BTreeSet<ArtifactScopeKey>,
}

fn collect_scope_requirements(
    id: ExplanationId,
    interpretations: &InterpretationStore,
    visiting: &mut BTreeSet<ExplanationId>,
    requirements: &mut BTreeMap<ArtifactId, ArtifactRequirement>,
) -> Result<(), ArtifactComparisonHydrationError> {
    if !visiting.insert(id) {
        return Err(ArtifactComparisonHydrationError::CyclicExplanation(id));
    }
    let definition = interpretations
        .explanation(id)
        .ok_or(ArtifactComparisonHydrationError::MissingExplanation(id))?;
    match &definition.scope {
        ExplanationScope::ArrangementClip(_) | ExplanationScope::PatternClip(_) => {}
        ExplanationScope::ReconstructionTrack { artifact, track } => add_requirement(
            requirements,
            *artifact,
            ArtifactKind::ReconstructionSet,
            ArtifactScopeKey::ReconstructionTrack(*track),
        )?,
        ExplanationScope::LoomSketch { artifact, clusters } => {
            for cluster in clusters {
                add_requirement(
                    requirements,
                    *artifact,
                    ArtifactKind::LoomSketch,
                    ArtifactScopeKey::LoomCluster(*cluster),
                )?;
            }
        }
        ExplanationScope::HpssComponent {
            artifact,
            component,
        } => add_requirement(
            requirements,
            *artifact,
            ArtifactKind::Hpss,
            ArtifactScopeKey::HpssComponent(*component),
        )?,
        ExplanationScope::ModelClaim { artifact, claim } => add_requirement(
            requirements,
            *artifact,
            ArtifactKind::ModelClaim,
            ArtifactScopeKey::ModelClaim(*claim),
        )?,
        ExplanationScope::Group(members) => {
            for member in members {
                collect_scope_requirements(*member, interpretations, visiting, requirements)?;
            }
        }
    }
    visiting.remove(&id);
    Ok(())
}

fn add_requirement(
    requirements: &mut BTreeMap<ArtifactId, ArtifactRequirement>,
    artifact: ArtifactId,
    kind: ArtifactKind,
    key: ArtifactScopeKey,
) -> Result<(), ArtifactComparisonHydrationError> {
    let entry = requirements
        .entry(artifact)
        .or_insert_with(|| ArtifactRequirement {
            kind: kind.clone(),
            keys: BTreeSet::new(),
        });
    if entry.kind != kind {
        return Err(ArtifactComparisonHydrationError::ConflictingArtifactKind(
            artifact,
        ));
    }
    entry.keys.insert(key);
    Ok(())
}

fn validate_pin(
    descriptor: &ArtifactDescriptor,
    pin: ArtifactComparisonPin,
) -> Result<(), ArtifactComparisonHydrationError> {
    descriptor
        .validate()
        .map_err(ArtifactComparisonHydrationError::Catalog)?;
    if pin.artifact != descriptor.id {
        return Err(ArtifactComparisonHydrationError::ArtifactIdentityMismatch {
            descriptor: descriptor.id,
            pinned: pin.artifact,
        });
    }
    if pin.source_digest != descriptor.source_digest {
        return Err(ArtifactComparisonHydrationError::SourceDigestMismatch(
            descriptor.id,
        ));
    }
    if pin.recipe_digest != descriptor.recipe_digest {
        return Err(ArtifactComparisonHydrationError::RecipeDigestMismatch(
            descriptor.id,
        ));
    }
    if pin.publication_generation == 0 || pin.catalog_generation == 0 {
        return Err(ArtifactComparisonHydrationError::ZeroGeneration(
            descriptor.id,
        ));
    }
    Ok(())
}

fn validate_context(
    descriptor: &ArtifactDescriptor,
    payload: &ArtifactComparisonPayload,
    context: ArtifactHydrationContext,
) -> Result<(), ArtifactComparisonHydrationError> {
    validate_pin(descriptor, payload.pin)?;
    if payload.pin.project_revisions != context.project_revisions {
        return Err(ArtifactComparisonHydrationError::StaleProjectRevision {
            artifact: descriptor.id,
            pinned: payload.pin.project_revisions,
            current: context.project_revisions,
        });
    }
    if payload.pin.publication_generation != context.publication_generation {
        return Err(ArtifactComparisonHydrationError::StalePublication {
            artifact: descriptor.id,
            pinned: payload.pin.publication_generation,
            current: context.publication_generation,
        });
    }
    if payload.pin.catalog_generation != context.catalog_generation {
        return Err(ArtifactComparisonHydrationError::StaleCatalog {
            artifact: descriptor.id,
            pinned: payload.pin.catalog_generation,
            current: context.catalog_generation,
        });
    }
    Ok(())
}

fn validate_signal(
    descriptor: &ArtifactDescriptor,
    signal: &ArtifactComparisonSignal,
) -> Result<(), ArtifactComparisonHydrationError> {
    if signal.origin_frame != descriptor.extent.start
        || signal.end_frame()? != descriptor.extent.end
    {
        return Err(ArtifactComparisonHydrationError::AlignmentMismatch {
            artifact: descriptor.id,
            descriptor: descriptor.extent,
            signal: FrameSpan {
                start: signal.origin_frame,
                end: signal.end_frame()?,
            },
        });
    }
    let format = signal.audio.format();
    if format.sample_rate.get() != descriptor.sample_rate {
        return Err(ArtifactComparisonHydrationError::SampleRateMismatch {
            descriptor: descriptor.sample_rate,
            signal: format.sample_rate.get(),
        });
    }
    if format.channels.get() != descriptor.channels {
        return Err(ArtifactComparisonHydrationError::ChannelMismatch {
            descriptor: descriptor.channels,
            signal: format.channels.get(),
        });
    }
    if let Some(index) = signal
        .audio
        .interleaved()
        .iter()
        .position(|sample| !sample.is_finite())
    {
        return Err(ArtifactComparisonHydrationError::NonFiniteSignal {
            artifact: descriptor.id,
            key: signal.key,
            index,
        });
    }
    Ok(())
}

fn signal_extent(
    descriptor: &ArtifactDescriptor,
) -> Result<ConcreteAspect, ArtifactComparisonHydrationError> {
    let channels = descriptor.channels;
    if channels > 16 {
        return Err(ArtifactComparisonHydrationError::TooManyChannels(channels));
    }
    let mask = ChannelMask(if channels == 16 {
        u16::MAX
    } else {
        (1_u16 << channels) - 1
    });
    let band = BandSpan::new(0.0, descriptor.sample_rate as f32 / 2.0)
        .ok_or_else(|| ArtifactComparisonHydrationError::Signal("invalid Nyquist band".into()))?;
    ConcreteAspect::new(
        vec![ConcreteRegion {
            time: descriptor.extent,
            band,
            channels: mask,
        }],
        SignalLayer::Source,
    )
    .map_err(|error| ArtifactComparisonHydrationError::Signal(error.to_string()))
}

fn require_kind(
    descriptor: &ArtifactDescriptor,
    expected: ArtifactKind,
) -> Result<(), ArtifactComparisonHydrationError> {
    if descriptor.kind == expected {
        Ok(())
    } else {
        Err(ArtifactComparisonHydrationError::KindMismatch {
            artifact: descriptor.id,
            expected,
            actual: descriptor.kind.clone(),
        })
    }
}

fn require_mono(descriptor: &ArtifactDescriptor) -> Result<(), ArtifactComparisonHydrationError> {
    if descriptor.channels == 1 {
        Ok(())
    } else {
        Err(ArtifactComparisonHydrationError::ChannelMismatch {
            descriptor: descriptor.channels,
            signal: 1,
        })
    }
}

#[derive(Clone, Debug)]
pub enum ArtifactComparisonHydrationError {
    Cancelled,
    MissingComparison(ComparisonId),
    MissingExplanation(ExplanationId),
    CyclicExplanation(ExplanationId),
    MissingArtifact(ArtifactId),
    PayloadTypeMismatch(ArtifactId),
    DescriptorConflict(ArtifactId),
    SignalConflict(ArtifactId),
    ConflictingArtifactKind(ArtifactId),
    ArtifactIdentityMismatch {
        descriptor: ArtifactId,
        pinned: ArtifactId,
    },
    SourceDigestMismatch(ArtifactId),
    RecipeDigestMismatch(ArtifactId),
    ZeroGeneration(ArtifactId),
    StaleProjectRevision {
        artifact: ArtifactId,
        pinned: ProjectRevisions,
        current: ProjectRevisions,
    },
    StalePublication {
        artifact: ArtifactId,
        pinned: u64,
        current: u64,
    },
    StaleCatalog {
        artifact: ArtifactId,
        pinned: u64,
        current: u64,
    },
    KindMismatch {
        artifact: ArtifactId,
        expected: ArtifactKind,
        actual: ArtifactKind,
    },
    DuplicateSignal(ArtifactScopeKey),
    MissingSignal {
        artifact: ArtifactId,
        key: ArtifactScopeKey,
    },
    SignalDigestMismatch {
        artifact: ArtifactId,
        key: ArtifactScopeKey,
    },
    EmptyPayload,
    AlignmentMismatch {
        artifact: ArtifactId,
        descriptor: FrameSpan,
        signal: FrameSpan,
    },
    SampleRateMismatch {
        descriptor: u32,
        signal: u32,
    },
    ChannelMismatch {
        descriptor: u16,
        signal: u16,
    },
    TooManyChannels(u16),
    NonFiniteSignal {
        artifact: ArtifactId,
        key: ArtifactScopeKey,
        index: usize,
    },
    SignalTooLarge,
    Signal(String),
    Catalog(ArtifactCatalogError),
    Explanation(crate::explanation::ExplanationError),
}

impl fmt::Display for ArtifactComparisonHydrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "artifact comparison hydration: {self:?}")
    }
}

impl Error for ArtifactComparisonHydrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(error) => Some(error),
            Self::Explanation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::explanation::ExplanationError> for ArtifactComparisonHydrationError {
    fn from(error: crate::explanation::ExplanationError) -> Self {
        Self::Explanation(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_catalog::{sha256_content, DigestAlgorithm};
    use crate::aspect::Aspect;
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::comparison::{ComparisonDefinition, SourceCitation};
    use crate::explanation::{ExplanationDefinition, ExplanationScope};
    use crate::hpss::{separate_harmonic_percussive, HpssSettings};
    use crate::interpretation::InterpretationCommand;
    use crate::loom::{ClusterTemplate, SequenceCluster, SequenceEvent};
    use crate::ontology::{Producer, Provenance};

    fn digest(value: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [value; 32])
    }

    fn revisions() -> ProjectRevisions {
        ProjectRevisions {
            aggregate: 12,
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

    fn context() -> ArtifactHydrationContext {
        ArtifactHydrationContext {
            project_revisions: revisions(),
            publication_generation: 21,
            catalog_generation: 34,
        }
    }

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Analyzer {
                name: "hydration-test".into(),
                version: "1".into(),
                configuration_digest: None,
            },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn descriptor(value: u8, kind: ArtifactKind, channels: u16) -> ArtifactDescriptor {
        ArtifactDescriptor {
            id: ArtifactId(digest(value)),
            kind,
            source_digest: digest(1),
            recipe_digest: digest(2),
            output_digest: digest(value),
            extent: FrameSpan::new(10, 14).unwrap(),
            sample_rate: 8_000,
            channels,
            provenance: provenance(),
        }
    }

    fn pin(descriptor: &ArtifactDescriptor) -> ArtifactComparisonPin {
        let context = context();
        ArtifactComparisonPin::from_descriptor(
            descriptor,
            context.project_revisions,
            context.publication_generation,
            context.catalog_generation,
        )
    }

    fn store(scope: ExplanationScope) -> Arc<InterpretationStore> {
        let explanation = ExplanationDefinition {
            id: ExplanationId(1),
            label: "artifact explanation".into(),
            scope,
            extent: Aspect::Time(FrameSpan::new(10, 14).unwrap()),
            evidence: Vec::new(),
            provenance: provenance(),
        };
        let comparison = ComparisonDefinition {
            id: ComparisonId(1),
            label: "artifact comparison".into(),
            source: SourceCitation {
                asset: AssetId(1),
                source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(4)).unwrap(),
                project_span: FrameSpan::new(10, 14).unwrap(),
                channels: ChannelMask(1),
            },
            explanation: explanation.id,
            provenance: provenance(),
        };
        let mut store = InterpretationStore::new();
        store
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(explanation),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(comparison),
                },
            ])
            .unwrap();
        Arc::new(store)
    }

    fn insert(
        descriptor: ArtifactDescriptor,
        payload: ArtifactComparisonPayload,
    ) -> ArtifactCatalog {
        let mut catalog = ArtifactCatalog::new();
        insert_artifact_comparison_payload(&mut catalog, descriptor, Arc::new(payload)).unwrap();
        catalog
    }

    fn rendered_signal(
        semantics: &ComparisonSemanticSnapshot,
        artifact: ArtifactId,
        key: ArtifactScopeKey,
    ) -> Vec<f32> {
        let payload = semantics
            .artifacts
            .get::<ArtifactExplanationPayload>(artifact)
            .unwrap();
        payload.signals[&key]
            .renderer
            .render(FrameSpan::new(10, 14).unwrap(), &RenderCancellation::new())
            .unwrap()
            .audio
            .interleaved()
            .to_vec()
    }

    #[test]
    fn hpss_payload_hydrates_real_complementary_components() {
        let descriptor = descriptor(10, ArtifactKind::Hpss, 1);
        let settings = HpssSettings {
            fft_size: 4,
            hop_size: 2,
            soft_mask_power: 2.0,
            time_median_width: 1,
            frequency_median_width: 1,
        };
        let result = separate_harmonic_percussive(&[1.0, -0.5, 0.25, 0.0], settings).unwrap();
        let payload = ArtifactComparisonPayload::from_hpss(
            &descriptor,
            pin(&descriptor),
            &result,
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(
            payload
                .signal(ArtifactScopeKey::HpssComponent(HpssComponentKind::Harmonic,))
                .unwrap()
                .audio
                .interleaved(),
            result.harmonic
        );
        assert_eq!(
            payload
                .signal(ArtifactScopeKey::HpssComponent(
                    HpssComponentKind::Percussive,
                ))
                .unwrap()
                .audio
                .interleaved(),
            result.percussive
        );
        let catalog = insert(descriptor.clone(), payload);
        let semantics = hydrate_comparison_semantics(
            &catalog,
            store(ExplanationScope::HpssComponent {
                artifact: descriptor.id,
                component: HpssComponentKind::Harmonic,
            }),
            ComparisonId(1),
            context(),
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(
            rendered_signal(
                &semantics,
                descriptor.id,
                ArtifactScopeKey::HpssComponent(HpssComponentKind::Harmonic)
            ),
            result.harmonic
        );
    }

    #[test]
    fn loom_payload_renders_each_requested_cluster_without_cross_family_pcm() {
        let descriptor = descriptor(11, ArtifactKind::LoomSketch, 1);
        let sketch = SequenceSketch {
            sample_rate: 8_000,
            clusters: vec![
                SequenceCluster {
                    template: ClusterTemplate {
                        cluster_id: 1,
                        samples: vec![1.0, 0.5],
                        onset_offset: 0,
                        medoid_event_id: 1,
                        exemplar_count: 1,
                        exemplar_agreement: 1.0,
                    },
                    enabled: true,
                    gain: 1.0,
                },
                SequenceCluster {
                    template: ClusterTemplate {
                        cluster_id: 2,
                        samples: vec![0.25, 0.25],
                        onset_offset: 0,
                        medoid_event_id: 2,
                        exemplar_count: 1,
                        exemplar_agreement: 1.0,
                    },
                    enabled: true,
                    gain: 1.0,
                },
            ],
            events: vec![
                SequenceEvent {
                    id: 1,
                    cluster_id: 1,
                    sample_index: 0,
                    gain: 1.0,
                    enabled: true,
                    salience: 1.0,
                    upstream_similarity: 1.0,
                    timing_adjustment: 0,
                    template_correlation: 1.0,
                },
                SequenceEvent {
                    id: 2,
                    cluster_id: 2,
                    sample_index: 2,
                    gain: 1.0,
                    enabled: true,
                    salience: 1.0,
                    upstream_similarity: 1.0,
                    timing_adjustment: 0,
                    template_correlation: 1.0,
                },
            ],
        };
        let payload = ArtifactComparisonPayload::from_loom(
            &descriptor,
            pin(&descriptor),
            &sketch,
            0,
            &RenderCancellation::new(),
        )
        .unwrap();
        let catalog = insert(descriptor.clone(), payload);
        let semantics = hydrate_comparison_semantics(
            &catalog,
            store(ExplanationScope::LoomSketch {
                artifact: descriptor.id,
                clusters: vec![1],
            }),
            ComparisonId(1),
            context(),
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(
            rendered_signal(&semantics, descriptor.id, ArtifactScopeKey::LoomCluster(1)),
            vec![1.0, 0.5, 0.0, 0.0]
        );
    }

    #[test]
    fn rhythm_and_model_claims_adopt_only_exact_rendered_pcm() {
        for (value, rhythm) in [(12_u8, true), (13_u8, false)] {
            let descriptor = descriptor(value, ArtifactKind::ModelClaim, 1);
            let claim = u64::from(value);
            let rendered = RenderedExplanation {
                origin_frame: 10,
                audio: ProjectAudio::from_interleaved(
                    AudioFormat::new(8_000, 1).unwrap(),
                    vec![0.1, 0.2, 0.3, 0.4],
                )
                .unwrap(),
            };
            let payload = if rhythm {
                ArtifactComparisonPayload::from_rhythm_render(
                    &descriptor,
                    pin(&descriptor),
                    claim,
                    rendered,
                    &RenderCancellation::new(),
                )
            } else {
                ArtifactComparisonPayload::from_model_render(
                    &descriptor,
                    pin(&descriptor),
                    claim,
                    rendered,
                    &RenderCancellation::new(),
                )
            }
            .unwrap();
            let catalog = insert(descriptor.clone(), payload);
            let semantics = hydrate_comparison_semantics(
                &catalog,
                store(ExplanationScope::ModelClaim {
                    artifact: descriptor.id,
                    claim,
                }),
                ComparisonId(1),
                context(),
                &RenderCancellation::new(),
            )
            .unwrap();
            assert_eq!(
                rendered_signal(
                    &semantics,
                    descriptor.id,
                    ArtifactScopeKey::ModelClaim(claim)
                ),
                vec![0.1, 0.2, 0.3, 0.4]
            );
        }
    }

    #[test]
    fn missing_invalidated_and_mismatched_artifacts_are_typed_refusals() {
        let descriptor = descriptor(14, ArtifactKind::ModelClaim, 1);
        let interpretations = store(ExplanationScope::ModelClaim {
            artifact: descriptor.id,
            claim: 14,
        });
        assert!(matches!(
            hydrate_comparison_semantics(
                &ArtifactCatalog::new(),
                Arc::clone(&interpretations),
                ComparisonId(1),
                context(),
                &RenderCancellation::new()
            ),
            Err(ArtifactComparisonHydrationError::MissingArtifact(_))
        ));

        let rendered = RenderedExplanation {
            origin_frame: 10,
            audio: ProjectAudio::from_interleaved(
                AudioFormat::new(8_000, 1).unwrap(),
                vec![0.0; 4],
            )
            .unwrap(),
        };
        let payload = ArtifactComparisonPayload::from_model_render(
            &descriptor,
            pin(&descriptor),
            14,
            rendered,
            &RenderCancellation::new(),
        )
        .unwrap();
        let catalog = insert(descriptor.clone(), payload);
        let mut stale = context();
        stale.catalog_generation += 1;
        assert!(matches!(
            hydrate_comparison_semantics(
                &catalog,
                interpretations,
                ComparisonId(1),
                stale,
                &RenderCancellation::new()
            ),
            Err(ArtifactComparisonHydrationError::StaleCatalog { .. })
        ));

        let mut mismatched = pin(&descriptor);
        mismatched.source_digest = sha256_content(b"wrong-source", &[]);
        let signal = ArtifactComparisonSignal::new(
            ArtifactScopeKey::ModelClaim(14),
            10,
            ProjectAudio::from_interleaved(AudioFormat::new(8_000, 1).unwrap(), vec![0.0; 4])
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ArtifactComparisonPayload::new(
                &descriptor,
                mismatched,
                [signal],
                &RenderCancellation::new()
            ),
            Err(ArtifactComparisonHydrationError::SourceDigestMismatch(_))
        ));

        let mut mismatched = pin(&descriptor);
        mismatched.recipe_digest = sha256_content(b"wrong-recipe", &[]);
        let signal = ArtifactComparisonSignal::new(
            ArtifactScopeKey::ModelClaim(14),
            10,
            ProjectAudio::from_interleaved(AudioFormat::new(8_000, 1).unwrap(), vec![0.0; 4])
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ArtifactComparisonPayload::new(
                &descriptor,
                mismatched,
                [signal],
                &RenderCancellation::new()
            ),
            Err(ArtifactComparisonHydrationError::RecipeDigestMismatch(_))
        ));
    }

    #[test]
    fn cancellation_refuses_payload_and_hydration_work() {
        let descriptor = descriptor(16, ArtifactKind::ModelClaim, 1);
        let cancellation = RenderCancellation::new();
        cancellation.cancel();
        let rendered = RenderedExplanation {
            origin_frame: 10,
            audio: ProjectAudio::from_interleaved(
                AudioFormat::new(8_000, 1).unwrap(),
                vec![0.0; 4],
            )
            .unwrap(),
        };
        assert!(matches!(
            ArtifactComparisonPayload::from_model_render(
                &descriptor,
                pin(&descriptor),
                16,
                rendered,
                &cancellation
            ),
            Err(ArtifactComparisonHydrationError::Cancelled)
        ));
        assert!(matches!(
            hydrate_comparison_semantics(
                &ArtifactCatalog::new(),
                store(ExplanationScope::ModelClaim {
                    artifact: descriptor.id,
                    claim: 16,
                }),
                ComparisonId(1),
                context(),
                &cancellation,
            ),
            Err(ArtifactComparisonHydrationError::Cancelled)
        ));
    }

    #[test]
    fn reconstruction_requires_exact_project_alignment() {
        let descriptor = descriptor(15, ArtifactKind::ReconstructionSet, 1);
        let track = ReconstructionTrackId::from_raw(9);
        let audio = ProjectAudio::from_interleaved(
            AudioFormat::new(8_000, 1).unwrap(),
            vec![0.2, 0.4, 0.6, 0.8],
        )
        .unwrap();
        assert!(matches!(
            ArtifactComparisonPayload::from_reconstruction_render(
                &descriptor,
                pin(&descriptor),
                track,
                RenderedExplanation {
                    origin_frame: 11,
                    audio: audio.clone()
                },
                &RenderCancellation::new()
            ),
            Err(ArtifactComparisonHydrationError::AlignmentMismatch { .. })
        ));
        let payload = ArtifactComparisonPayload::from_reconstruction_render(
            &descriptor,
            pin(&descriptor),
            track,
            RenderedExplanation {
                origin_frame: 10,
                audio,
            },
            &RenderCancellation::new(),
        )
        .unwrap();
        let catalog = insert(descriptor.clone(), payload);
        let semantics = hydrate_comparison_semantics(
            &catalog,
            store(ExplanationScope::ReconstructionTrack {
                artifact: descriptor.id,
                track,
            }),
            ComparisonId(1),
            context(),
            &RenderCancellation::new(),
        )
        .unwrap();
        assert_eq!(
            rendered_signal(
                &semantics,
                descriptor.id,
                ArtifactScopeKey::ReconstructionTrack(track)
            ),
            vec![0.2, 0.4, 0.6, 0.8]
        );
    }
}
