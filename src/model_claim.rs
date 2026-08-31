//! Immutable provenance for material published by a model worker.
//!
//! A claim records what one exact model recipe produced about one exact source
//! span. It is intentionally not a mixer track, instrument identity, or an
//! acceptance decision. UI/AIR/reconstruction bridges can attach their own
//! objects to these stable references without copying a friendly model label.

use std::collections::BTreeSet;
use std::fmt;

use crate::model_wire::{AdditivityDeclaration, ArtifactDescriptor, ArtifactKind, WorkerResult};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelClaimId(String);

impl ModelClaimId {
    /// A claim ID is content-derived from the immutable worker cache key. It
    /// is not a source identity and must be namespaced by project/reading on
    /// import, just like other portable local IDs.
    pub fn from_cache_key(cache_key: impl Into<String>) -> Result<Self, ClaimError> {
        let value = cache_key.into();
        validate_sha256("claim cache_key", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimSource {
    /// SHA-256 of the exact decoded material representation handed to worker.
    pub material_sha256: String,
    pub start_frame: u64,
    pub frame_count: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl ClaimSource {
    pub fn validate(&self) -> Result<(), ClaimError> {
        validate_sha256("claim source material", &self.material_sha256)?;
        if self.frame_count == 0 || self.start_frame.checked_add(self.frame_count).is_none() {
            return Err(ClaimError::Invalid(
                "claim source must have a non-empty non-overflowing frame span".into(),
            ));
        }
        if self.sample_rate_hz == 0 || self.channels == 0 {
            return Err(ClaimError::Invalid(
                "claim source rate and channels must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimConfidenceKind {
    /// No claim that the number is calibrated across material/models.
    RelativeSupport,
    CalibratedProbability,
    Logit,
    RankingScore,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelLabel {
    /// Exactly the adapter/model vocabulary, e.g. `kick` or `vocal`.
    pub model_authored_label: String,
    pub ontology: String,
    pub ontology_revision: u32,
    pub confidence_kind: ClaimConfidenceKind,
    pub confidence: Option<f32>,
}

impl ModelLabel {
    pub fn validate(&self) -> Result<(), ClaimError> {
        validate_label("model label", &self.model_authored_label)?;
        validate_label("model label ontology", &self.ontology)?;
        if self.ontology_revision == 0 {
            return Err(ClaimError::Invalid(
                "model label ontology revision must be non-zero".into(),
            ));
        }
        if let Some(value) = self.confidence {
            if !value.is_finite() {
                return Err(ClaimError::Invalid(
                    "model-label confidence must be finite".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRuntimeProvenance {
    pub worker_name: String,
    /// Version-pinned runtime string from the manifest/worker handshake.
    pub runtime: String,
    /// Optional hash of the reviewed adapter executable or source bundle.
    pub adapter_sha256: Option<String>,
}

impl WorkerRuntimeProvenance {
    pub fn validate(&self) -> Result<(), ClaimError> {
        validate_label("worker name", &self.worker_name)?;
        validate_label("worker runtime", &self.runtime)?;
        if let Some(hash) = &self.adapter_sha256 {
            validate_sha256("adapter hash", hash)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelClaimArtifact {
    pub descriptor: ArtifactDescriptor,
    /// The model authored name is retained separate from the artifact path.
    pub output_name: String,
    pub labels: Vec<ModelLabel>,
}

impl ModelClaimArtifact {
    pub fn validate(&self) -> Result<(), ClaimError> {
        validate_label("claim output name", &self.output_name)?;
        if self.descriptor.schema_revision == 0 || self.descriptor.relative_path.is_empty() {
            return Err(ClaimError::Invalid(
                "claim artifact needs a typed descriptor".into(),
            ));
        }
        validate_sha256("claim artifact digest", &self.descriptor.sha256)?;
        for label in &self.labels {
            label.validate()?;
        }
        Ok(())
    }

    pub fn is_audio(&self) -> bool {
        matches!(self.descriptor.kind, ArtifactKind::Audio)
    }
}

/// One immutable output bundle from an isolated worker execution.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelClaimBundle {
    pub id: ModelClaimId,
    pub model_manifest_sha256: String,
    pub cache_key: String,
    pub source: ClaimSource,
    pub runtime: WorkerRuntimeProvenance,
    /// Claim-level semantics used when no output-specific stronger statement
    /// is available. Individual artifacts always retain their own declaration.
    pub additivity: AdditivityDeclaration,
    pub artifacts: Vec<ModelClaimArtifact>,
    /// Measurements are preserved as raw typed sidecar names; interpretation
    /// belongs to an adapter-specific/AIR bridge rather than this registry.
    pub measurement_artifact_names: BTreeSet<String>,
}

impl ModelClaimBundle {
    pub fn new(
        model_manifest_sha256: String,
        cache_key: String,
        source: ClaimSource,
        runtime: WorkerRuntimeProvenance,
        additivity: AdditivityDeclaration,
        artifacts: Vec<ModelClaimArtifact>,
    ) -> Result<Self, ClaimError> {
        validate_sha256("model manifest", &model_manifest_sha256)?;
        let id = ModelClaimId::from_cache_key(cache_key.clone())?;
        let claim = Self {
            id,
            cache_key,
            model_manifest_sha256,
            source,
            runtime,
            additivity,
            artifacts,
            measurement_artifact_names: BTreeSet::new(),
        };
        claim.validate()?;
        Ok(claim)
    }

    pub fn from_worker_result(
        manifest_sha256: impl Into<String>,
        source: ClaimSource,
        runtime: WorkerRuntimeProvenance,
        additivity: AdditivityDeclaration,
        result: WorkerResult,
        outputs: Vec<(String, Vec<ModelLabel>)>,
    ) -> Result<Self, ClaimError> {
        let manifest_sha256 = manifest_sha256.into();
        validate_sha256("model manifest", &manifest_sha256)?;
        let id = ModelClaimId::from_cache_key(result.cache_key.clone())?;
        if outputs.len() != result.artifacts.len() {
            return Err(ClaimError::Invalid(
                "each staged worker artifact needs an explicit output name/label set".into(),
            ));
        }
        let artifacts = result
            .artifacts
            .into_iter()
            .zip(outputs)
            .map(|(descriptor, (output_name, labels))| ModelClaimArtifact {
                descriptor,
                output_name,
                labels,
            })
            .collect();
        let measurement_artifact_names = result
            .measurements
            .into_iter()
            .map(|measurement| measurement.name)
            .collect();
        let claim = Self {
            id,
            model_manifest_sha256: manifest_sha256,
            cache_key: result.cache_key,
            source,
            runtime,
            additivity,
            artifacts,
            measurement_artifact_names,
        };
        claim.validate()?;
        Ok(claim)
    }

    pub fn validate(&self) -> Result<(), ClaimError> {
        validate_sha256("model manifest", &self.model_manifest_sha256)?;
        validate_sha256("claim cache key", &self.cache_key)?;
        if self.id.as_str() != self.cache_key {
            return Err(ClaimError::Invalid(
                "claim ID must equal the worker cache key".into(),
            ));
        }
        self.source.validate()?;
        self.runtime.validate()?;
        if self.artifacts.is_empty() {
            return Err(ClaimError::Invalid(
                "model claim must retain at least one artifact".into(),
            ));
        }
        let mut names = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for artifact in &self.artifacts {
            artifact.validate()?;
            if !names.insert(&artifact.output_name)
                || !paths.insert(&artifact.descriptor.relative_path)
            {
                return Err(ClaimError::Invalid(
                    "claim artifact output names and paths must be unique".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn artifact(&self, output_name: &str) -> Option<&ModelClaimArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.output_name == output_name)
    }

    /// Stable opaque locator for `reconstruction::AnalysisProvenance` today;
    /// a later AIR bridge should preserve this exact claim/artifact pair.
    pub fn analysis_locator(&self, output_name: &str) -> Option<String> {
        self.artifact(output_name).map(|artifact| {
            format!(
                "model-claim/{}/{}@{}",
                self.id.as_str(),
                artifact.output_name,
                artifact.descriptor.sha256
            )
        })
    }
}

#[derive(Debug)]
pub enum ClaimError {
    Invalid(String),
}
impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(detail) => f.write_str(detail),
        }
    }
}
impl std::error::Error for ClaimError {}

fn validate_sha256(field: &str, value: &str) -> Result<(), ClaimError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ClaimError::Invalid(format!(
            "{field} must be a lowercase SHA-256"
        )));
    }
    Ok(())
}
fn validate_label(field: &str, value: &str) -> Result<(), ClaimError> {
    if value.is_empty()
        || value.len() > 160
        || value.contains(char::is_whitespace)
        || value.contains(['/', '\\', '\0'])
    {
        return Err(ClaimError::Invalid(format!(
            "{field} must be a compact non-path label"
        )));
    }
    Ok(())
}
