//! Persistent comparison definitions and exact-alignment signal products.
//!
//! A comparison records an experiment: an explicit source citation against a
//! persistent explanation. Its measurements describe signal reconstruction,
//! not whether the explanation is causally, musically, or perceptually right.
//! Residual subtraction is sample-aligned and never gain-fitted.

use std::fmt;

use crate::artifact_catalog::{ArtifactCatalog, ContentDigest};
use crate::aspect::{ChannelMask, FrameSpan};
use crate::assets::{AssetFrameRange, AssetId};
use crate::audio::ProjectAudio;
use crate::daw_project::ProjectRevisions;
use crate::explanation::{ExplanationDependencyPin, ExplanationId, RenderedExplanation};
use crate::ontology;
use crate::render_validation::GoldenFingerprint;

const ENERGY_FLOOR: f64 = 1.0e-20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComparisonId(pub u64);

/// Exact mapping between immutable asset frames and the project window where
/// an explanation is tested. Resampling/channel projection remain explicit
/// convergence-layer operations; they are never inferred from a file path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceCitation {
    pub asset: AssetId,
    pub source_range: AssetFrameRange,
    pub project_span: FrameSpan,
    pub channels: ChannelMask,
}

impl SourceCitation {
    pub fn validate(self) -> Result<(), ComparisonError> {
        if self.asset.0 == 0 {
            return Err(ComparisonError::InvalidSource(
                "asset identity is zero".into(),
            ));
        }
        if self.source_range.start >= self.source_range.end {
            return Err(ComparisonError::InvalidSource(
                "source range is empty or reversed".into(),
            ));
        }
        if self.project_span.start >= self.project_span.end {
            return Err(ComparisonError::InvalidSource(
                "project span is empty or reversed".into(),
            ));
        }
        if self.channels.is_empty() {
            return Err(ComparisonError::InvalidSource(
                "channel selection is empty".into(),
            ));
        }
        Ok(())
    }
}

/// Durable comparison intent. Rendered buffers live only in caches/services.
#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonDefinition {
    pub id: ComparisonId,
    pub label: String,
    pub source: SourceCitation,
    pub explanation: ExplanationId,
    pub provenance: ontology::Provenance,
}

impl ComparisonDefinition {
    pub fn validate(&self) -> Result<(), ComparisonError> {
        if self.id.0 == 0 {
            return Err(ComparisonError::ZeroIdentity);
        }
        if self.label.trim().is_empty() {
            return Err(ComparisonError::EmptyLabel);
        }
        if self.explanation.0 == 0 {
            return Err(ComparisonError::MissingExplanation(self.explanation));
        }
        self.source.validate()
    }
}

/// Collision-resistant local render identity, distinct from the deliberately
/// tolerant [`GoldenFingerprint`] used for portable verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExactRenderDigest(pub ContentDigest);

impl ExactRenderDigest {
    pub fn new(digest: ContentDigest) -> Result<Self, ComparisonError> {
        if digest.is_strong() {
            Ok(Self(digest))
        } else {
            Err(ComparisonError::WeakRenderDigest)
        }
    }
}

/// Signal-only measurements. No field here is named quality, correctness, or
/// confidence; those belong to AIR claims and attributed human judgment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ComparisonMetrics {
    pub sample_count: u64,
    pub source_energy: f64,
    pub construction_energy: f64,
    pub residual_energy: f64,
    pub normalized_residual_energy: f64,
    /// May be negative when the construction makes the null worse.
    pub signed_explained_energy: f64,
    pub clamped_explained_energy: f64,
    /// Sample-domain energy surplus. Time-frequency excess is in coverage.
    pub excess_energy_ratio: f64,
    pub correlation: f64,
    /// Least-squares diagnostic only. It was not applied to the residual.
    pub suggested_gain: f64,
    pub quarantined_source_samples: u64,
    pub quarantined_construction_samples: u64,
}

/// Last explicitly recorded observation. Refresh replaces this atomically;
/// unresolved comparisons retain their previous observation as history.
#[derive(Clone, Debug, PartialEq)]
pub struct ComparisonObservation {
    pub dependencies: ExplanationDependencyPin,
    pub source_digest: ExactRenderDigest,
    pub construction_digest: ExactRenderDigest,
    pub residual_digest: ExactRenderDigest,
    pub construction_fingerprint: GoldenFingerprint,
    pub residual_fingerprint: GoldenFingerprint,
    pub metrics: ComparisonMetrics,
}

impl ComparisonObservation {
    pub fn status(
        &self,
        revisions: ProjectRevisions,
        artifacts: &ArtifactCatalog,
    ) -> ComparisonStatus {
        if self.dependencies.is_stale(revisions) {
            return ComparisonStatus::StaleProject;
        }
        let missing = self
            .dependencies
            .artifacts
            .iter()
            .copied()
            .filter(|id| artifacts.descriptor(*id).is_none())
            .collect::<Vec<_>>();
        if missing.is_empty() {
            ComparisonStatus::Current
        } else {
            ComparisonStatus::MissingArtifacts(missing)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComparisonStatus {
    NeverRendered,
    Current,
    StaleProject,
    MissingArtifacts(Vec<crate::artifact_catalog::ArtifactId>),
    Unresolvable(String),
}

/// Ephemeral aligned audio used by audition and coverage computation.
#[derive(Clone, Debug)]
pub struct RenderedComparison {
    pub origin_frame: i64,
    pub source: ProjectAudio,
    pub construction: ProjectAudio,
    pub residual: ProjectAudio,
    pub metrics: ComparisonMetrics,
}

/// Form `source - construction` without fitting either operand. Non-finite
/// values are counted and quarantined to zero at this boundary.
///
/// "Exact" here means exact frame/channel alignment and no hidden optimizer.
/// IEEE addition does not guarantee bitwise `construction + residual ==
/// source`; callers use an explicit numerical null tolerance for that check.
pub fn render_comparison(
    source_origin: i64,
    source: ProjectAudio,
    construction: RenderedExplanation,
) -> Result<RenderedComparison, ComparisonError> {
    if source_origin != construction.origin_frame {
        return Err(ComparisonError::OriginMismatch {
            source: source_origin,
            construction: construction.origin_frame,
        });
    }
    if source.format() != construction.audio.format()
        || source.frame_count() != construction.audio.frame_count()
    {
        return Err(ComparisonError::FormatMismatch);
    }

    let mut clean_source = Vec::with_capacity(source.interleaved().len());
    let mut clean_construction = Vec::with_capacity(source.interleaved().len());
    let mut residual = Vec::with_capacity(source.interleaved().len());
    let mut source_invalid = 0_u64;
    let mut construction_invalid = 0_u64;
    for (&source_sample, &construction_sample) in source
        .interleaved()
        .iter()
        .zip(construction.audio.interleaved())
    {
        let source_sample = if source_sample.is_finite() {
            source_sample
        } else {
            source_invalid += 1;
            0.0
        };
        let construction_sample = if construction_sample.is_finite() {
            construction_sample
        } else {
            construction_invalid += 1;
            0.0
        };
        clean_source.push(source_sample);
        clean_construction.push(construction_sample);
        residual.push(source_sample - construction_sample);
    }
    let metrics = measure_comparison(
        &clean_source,
        &clean_construction,
        &residual,
        source_invalid,
        construction_invalid,
    );
    let format = source.format();
    Ok(RenderedComparison {
        origin_frame: source_origin,
        source: ProjectAudio::from_interleaved(format, clean_source)
            .map_err(|error| ComparisonError::Audio(error.to_string()))?,
        construction: ProjectAudio::from_interleaved(format, clean_construction)
            .map_err(|error| ComparisonError::Audio(error.to_string()))?,
        residual: ProjectAudio::from_interleaved(format, residual)
            .map_err(|error| ComparisonError::Audio(error.to_string()))?,
        metrics,
    })
}

fn measure_comparison(
    source: &[f32],
    construction: &[f32],
    residual: &[f32],
    source_invalid: u64,
    construction_invalid: u64,
) -> ComparisonMetrics {
    let mut source_energy = 0.0;
    let mut construction_energy = 0.0;
    let mut residual_energy = 0.0;
    let mut dot = 0.0;
    for ((&source, &construction), &residual) in source.iter().zip(construction).zip(residual) {
        let source = f64::from(source);
        let construction = f64::from(construction);
        let residual = f64::from(residual);
        source_energy += source * source;
        construction_energy += construction * construction;
        residual_energy += residual * residual;
        dot += source * construction;
    }
    let normalized = if source_energy <= ENERGY_FLOOR {
        if residual_energy <= ENERGY_FLOOR {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        residual_energy / source_energy
    };
    let signed_explained = 1.0 - normalized;
    let correlation = if source_energy <= ENERGY_FLOOR || construction_energy <= ENERGY_FLOOR {
        0.0
    } else {
        dot / (source_energy * construction_energy).sqrt()
    };
    let suggested_gain = if construction_energy <= ENERGY_FLOOR {
        0.0
    } else {
        dot / construction_energy
    };
    ComparisonMetrics {
        sample_count: source.len() as u64,
        source_energy,
        construction_energy,
        residual_energy,
        normalized_residual_energy: normalized,
        signed_explained_energy: signed_explained,
        clamped_explained_energy: signed_explained.clamp(0.0, 1.0),
        excess_energy_ratio: (construction_energy - source_energy).max(0.0)
            / source_energy.max(ENERGY_FLOOR),
        correlation,
        suggested_gain,
        quarantined_source_samples: source_invalid,
        quarantined_construction_samples: construction_invalid,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComparisonError {
    ZeroIdentity,
    EmptyLabel,
    MissingExplanation(ExplanationId),
    InvalidSource(String),
    WeakRenderDigest,
    OriginMismatch { source: i64, construction: i64 },
    FormatMismatch,
    Audio(String),
}

impl fmt::Display for ComparisonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroIdentity => formatter.write_str("comparison identity must be non-zero"),
            Self::EmptyLabel => formatter.write_str("comparison label must not be empty"),
            Self::MissingExplanation(id) => write!(formatter, "explanation {} is missing", id.0),
            Self::InvalidSource(message) => {
                write!(formatter, "invalid comparison source: {message}")
            }
            Self::WeakRenderDigest => {
                formatter.write_str("exact render cache identity requires a strong digest")
            }
            Self::OriginMismatch {
                source,
                construction,
            } => write!(
                formatter,
                "source origin {source} differs from construction origin {construction}"
            ),
            Self::FormatMismatch => {
                formatter.write_str("source and construction formats or lengths differ")
            }
            Self::Audio(message) => write!(formatter, "comparison audio error: {message}"),
        }
    }
}

impl std::error::Error for ComparisonError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioFormat;

    #[test]
    fn residual_is_unfitted_and_over_explanation_remains_visible() {
        let format = AudioFormat::new(48_000, 1).unwrap();
        let source = ProjectAudio::from_interleaved(format, vec![1.0, -0.5]).unwrap();
        let construction = RenderedExplanation {
            origin_frame: 10,
            audio: ProjectAudio::from_interleaved(format, vec![2.0, -1.0]).unwrap(),
        };
        let rendered = render_comparison(10, source, construction).unwrap();
        assert_eq!(rendered.residual.interleaved(), &[-1.0, 0.5]);
        assert_eq!(rendered.metrics.suggested_gain, 0.5);
        assert!(rendered.metrics.excess_energy_ratio > 0.0);
        assert_eq!(rendered.metrics.clamped_explained_energy, 0.0);
    }

    #[test]
    fn non_finite_values_are_counted_and_quarantined() {
        let format = AudioFormat::new(48_000, 1).unwrap();
        let source = ProjectAudio::from_interleaved(format, vec![f32::NAN, 1.0]).unwrap();
        let construction = RenderedExplanation {
            origin_frame: 0,
            audio: ProjectAudio::from_interleaved(format, vec![0.0, f32::INFINITY]).unwrap(),
        };
        let rendered = render_comparison(0, source, construction).unwrap();
        assert_eq!(rendered.source.interleaved(), &[0.0, 1.0]);
        assert_eq!(rendered.construction.interleaved(), &[0.0, 0.0]);
        assert_eq!(rendered.metrics.quarantined_source_samples, 1);
        assert_eq!(rendered.metrics.quarantined_construction_samples, 1);
    }
}
