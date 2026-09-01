//! Authoritative recipes for optional model inference.
//!
//! A recipe owns the complete meaning of one computation: exact material,
//! effective parameters, expected named outputs, and publication semantics.
//! Workers may produce bytes, but cannot choose cache identity or reinterpret
//! those bytes. A result becomes a model claim only after the stored bytes and
//! their descriptors validate against this recipe.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path};
use std::sync::Arc;

use crate::model_claim::{
    ClaimSource, ModelClaimArtifact, ModelClaimBundle, ModelLabel, WorkerRuntimeProvenance,
};
use crate::model_store::StoredResult;
use crate::model_task_service::{ModelTaskRecipe, TaskMaterial};
use crate::model_wire::{
    AdditivityDeclaration, AnalyzeRequest, ArtifactDescriptor, ArtifactKind, JobFiles,
    WireParameter,
};
use crate::model_worker::{sha256_bytes, CacheKeyBuilder, ContentHash};
use crate::worker_runtime::ClaimPublication;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InferenceRecipeId(ContentHash);

impl InferenceRecipeId {
    pub const fn digest(self) -> ContentHash {
        self.0
    }
}

impl fmt::Display for InferenceRecipeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Exact bytes passed to a worker, paired with a digest computed here rather
/// than trusted from a UI, decoder, or adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedMaterial {
    bytes: Arc<[u8]>,
    sha256: ContentHash,
}

impl VerifiedMaterial {
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        let bytes = bytes.into();
        let sha256 = sha256_bytes(&bytes);
        Self { bytes, sha256 }
    }

    pub fn from_claimed_bytes(
        bytes: impl Into<Arc<[u8]>>,
        claimed: ContentHash,
    ) -> Result<Self, RecipeError> {
        let material = Self::from_bytes(bytes);
        if material.sha256 != claimed {
            return Err(RecipeError::MaterialDigestMismatch {
                expected: claimed,
                actual: material.sha256,
            });
        }
        Ok(material)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn sha256(&self) -> ContentHash {
        self.sha256
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpectedInferenceOutput {
    /// Adapter-authored semantic name, independent of its storage path.
    pub name: String,
    /// Stable worker ABI slot. Storage paths are never inferred from names.
    pub relative_path: String,
    pub kind: ArtifactKind,
    pub media_type: String,
    pub schema_revision: u32,
    pub time_base_hz: Option<u32>,
    pub additivity: AdditivityDeclaration,
    pub labels: Vec<ModelLabel>,
}

#[derive(Clone, Debug)]
pub struct InferenceRecipeSpec {
    pub model_manifest_sha256: ContentHash,
    pub material: VerifiedMaterial,
    pub start_frame: u64,
    pub frame_count: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub channel_selection: Vec<u16>,
    pub prompt: Option<String>,
    pub reference_sha256: Vec<ContentHash>,
    pub mask_sha256: Vec<ContentHash>,
    pub parameters: BTreeMap<String, WireParameter>,
    pub runtime: WorkerRuntimeProvenance,
    pub additivity: AdditivityDeclaration,
    pub outputs: Vec<ExpectedInferenceOutput>,
    pub measurement_names: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct InferenceRecipe {
    id: InferenceRecipeId,
    spec: InferenceRecipeSpec,
}

impl InferenceRecipe {
    pub fn new(mut spec: InferenceRecipeSpec) -> Result<Self, RecipeError> {
        validate_spec(&mut spec)?;
        let id = InferenceRecipeId(compute_recipe_hash(&spec));
        Ok(Self { id, spec })
    }

    pub const fn id(&self) -> InferenceRecipeId {
        self.id
    }

    pub fn spec(&self) -> &InferenceRecipeSpec {
        &self.spec
    }

    pub fn material(&self) -> &VerifiedMaterial {
        &self.spec.material
    }

    /// Constructs the worker request from recipe-owned facts. The caller may
    /// choose only ephemeral job paths and identity.
    pub fn analyze_request(&self, job_id: String, files: JobFiles) -> AnalyzeRequest {
        AnalyzeRequest {
            job_id,
            model_manifest_sha256: self.spec.model_manifest_sha256.to_string(),
            cache_key: self.id.to_string(),
            material_sha256: self.spec.material.sha256().to_string(),
            start_frame: self.spec.start_frame,
            frame_count: self.spec.frame_count,
            channel_selection: self.spec.channel_selection.clone(),
            prompt: self.spec.prompt.clone(),
            reference_sha256: self
                .spec
                .reference_sha256
                .iter()
                .map(ToString::to_string)
                .collect(),
            mask_sha256: self
                .spec
                .mask_sha256
                .iter()
                .map(ToString::to_string)
                .collect(),
            parameters: self.spec.parameters.clone(),
            files,
        }
    }

    /// Compatibility bridge for the existing task service. Every identity,
    /// material, and publication field is derived from this recipe; callers
    /// supply only the registry's stable model ID.
    pub fn model_task_recipe(&self, model_id: impl Into<String>) -> ModelTaskRecipe {
        ModelTaskRecipe {
            model_id: model_id.into(),
            cache_key: self.id.to_string(),
            material: TaskMaterial {
                sha256: self.spec.material.sha256().to_string(),
                bytes: self.spec.material.bytes().to_vec(),
                start_frame: self.spec.start_frame,
                frame_count: self.spec.frame_count,
                channel_selection: self.spec.channel_selection.clone(),
            },
            prompt: self.spec.prompt.clone(),
            reference_sha256: self
                .spec
                .reference_sha256
                .iter()
                .map(ToString::to_string)
                .collect(),
            mask_sha256: self
                .spec
                .mask_sha256
                .iter()
                .map(ToString::to_string)
                .collect(),
            parameters: self.spec.parameters.clone(),
            publication: ClaimPublication {
                model_manifest_sha256: self.spec.model_manifest_sha256.to_string(),
                source: ClaimSource {
                    material_sha256: self.spec.material.sha256().to_string(),
                    start_frame: self.spec.start_frame,
                    frame_count: self.spec.frame_count,
                    sample_rate_hz: self.spec.sample_rate_hz,
                    channels: self.spec.channels,
                },
                runtime: self.spec.runtime.clone(),
                additivity: self.spec.additivity.clone(),
                outputs: self
                    .spec
                    .outputs
                    .iter()
                    .map(|output| (output.name.clone(), output.labels.clone()))
                    .collect(),
            },
        }
    }

    /// Re-verifies an immutable store entry and constructs a claim using only
    /// recipe-owned names and semantics. Recipe-bound paths are stable output
    /// slots, so worker emission order cannot relabel an artifact.
    pub fn validate_stored(&self, stored: StoredResult) -> Result<StoredResultBundle, RecipeError> {
        if stored.result.cache_key != self.id.to_string() {
            return Err(RecipeError::StoredRecipeMismatch);
        }
        if stored.result.artifacts.len() != self.spec.outputs.len() {
            return Err(RecipeError::OutputCount {
                expected: self.spec.outputs.len(),
                actual: stored.result.artifacts.len(),
            });
        }

        let actual_measurements: BTreeSet<_> = stored
            .result
            .measurements
            .iter()
            .map(|measurement| measurement.name.clone())
            .collect();
        if actual_measurements.len() != stored.result.measurements.len()
            || actual_measurements != self.spec.measurement_names
        {
            return Err(RecipeError::MeasurementShape);
        }

        let mut stored_by_path = BTreeMap::new();
        for descriptor in &stored.result.artifacts {
            if stored_by_path
                .insert(descriptor.relative_path.as_str(), descriptor)
                .is_some()
            {
                return Err(RecipeError::StoredArtifact(
                    descriptor.relative_path.clone(),
                ));
            }
        }
        let mut claim_artifacts = Vec::with_capacity(self.spec.outputs.len());
        for (index, expected) in self.spec.outputs.iter().enumerate() {
            let descriptor = stored_by_path
                .get(expected.relative_path.as_str())
                .ok_or_else(|| RecipeError::MissingOutput {
                    name: expected.name.clone(),
                    relative_path: expected.relative_path.clone(),
                })?;
            validate_descriptor(index, descriptor, expected)?;
            validate_backlinks(self, descriptor)?;
            verify_stored_bytes(&stored.directory, descriptor)?;
            claim_artifacts.push(ModelClaimArtifact {
                descriptor: (*descriptor).clone(),
                output_name: expected.name.clone(),
                labels: expected.labels.clone(),
            });
        }

        let source = ClaimSource {
            material_sha256: self.spec.material.sha256().to_string(),
            start_frame: self.spec.start_frame,
            frame_count: self.spec.frame_count,
            sample_rate_hz: self.spec.sample_rate_hz,
            channels: self.spec.channels,
        };
        let mut claim = ModelClaimBundle::new(
            self.spec.model_manifest_sha256.to_string(),
            self.id.to_string(),
            source,
            self.spec.runtime.clone(),
            self.spec.additivity.clone(),
            claim_artifacts,
        )
        .map_err(|error| RecipeError::Claim(error.to_string()))?;
        claim.measurement_artifact_names = self.spec.measurement_names.clone();
        claim
            .validate()
            .map_err(|error| RecipeError::Claim(error.to_string()))?;
        Ok(StoredResultBundle {
            recipe_id: self.id,
            stored,
            claim,
        })
    }
}

#[derive(Clone, Debug)]
pub struct StoredResultBundle {
    pub recipe_id: InferenceRecipeId,
    pub stored: StoredResult,
    pub claim: ModelClaimBundle,
}

fn validate_spec(spec: &mut InferenceRecipeSpec) -> Result<(), RecipeError> {
    if spec.frame_count == 0 || spec.start_frame.checked_add(spec.frame_count).is_none() {
        return Err(RecipeError::Invalid(
            "source span must be non-empty and non-overflowing",
        ));
    }
    if spec.sample_rate_hz == 0 || spec.channels == 0 {
        return Err(RecipeError::Invalid("source format must be non-zero"));
    }
    let selected_channels: BTreeSet<_> = spec.channel_selection.iter().copied().collect();
    if selected_channels.len() != spec.channel_selection.len()
        || spec
            .channel_selection
            .iter()
            .any(|channel| *channel >= spec.channels)
    {
        return Err(RecipeError::Invalid(
            "selected channels must be unique and inside the source format",
        ));
    }
    if spec.outputs.is_empty() {
        return Err(RecipeError::Invalid(
            "at least one named output is required",
        ));
    }
    spec.outputs
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let mut output_names = BTreeSet::new();
    let mut output_paths = BTreeSet::new();
    for output in &spec.outputs {
        validate_name("output", &output.name)?;
        if !output_names.insert(output.name.clone()) {
            return Err(RecipeError::Invalid("output names must be unique"));
        }
        validate_relative_path(&output.relative_path)?;
        if !output_paths.insert(output.relative_path.clone()) {
            return Err(RecipeError::Invalid("output paths must be unique"));
        }
        if output.media_type.trim().is_empty() || output.schema_revision == 0 {
            return Err(RecipeError::Invalid(
                "each output needs media type and schema revision",
            ));
        }
        if matches!(output.kind, ArtifactKind::Audio) && output.time_base_hz.unwrap_or(0) == 0 {
            return Err(RecipeError::Invalid("audio outputs need a time base"));
        }
        for label in &output.labels {
            label
                .validate()
                .map_err(|error| RecipeError::Claim(error.to_string()))?;
        }
        if output.additivity != spec.additivity {
            return Err(RecipeError::Invalid(
                "output additivity must agree with the bundle declaration",
            ));
        }
    }
    if let AdditivityDeclaration::LinearSumWithResidual { residual_artifact } = &spec.additivity {
        if !output_names.contains(residual_artifact) {
            return Err(RecipeError::Invalid(
                "the declared residual must name an expected output",
            ));
        }
    }
    if matches!(spec.additivity, AdditivityDeclaration::NonAudio)
        && spec
            .outputs
            .iter()
            .any(|output| matches!(output.kind, ArtifactKind::Audio))
    {
        return Err(RecipeError::Invalid(
            "a non-audio bundle cannot declare audio outputs",
        ));
    }
    for (name, value) in &spec.parameters {
        validate_name("recipe field", name)?;
        if matches!(value, WireParameter::Float(value) if !value.is_finite()) {
            return Err(RecipeError::Invalid(
                "floating-point parameters must be finite",
            ));
        }
    }
    for name in &spec.measurement_names {
        validate_name("recipe field", name)?;
    }
    spec.runtime
        .validate()
        .map_err(|error| RecipeError::Claim(error.to_string()))?;
    Ok(())
}

fn compute_recipe_hash(spec: &InferenceRecipeSpec) -> ContentHash {
    let mut builder = CacheKeyBuilder::new();
    builder.add_str("recipe.schema", "audec.inference-recipe.v1");
    builder.add_bytes("manifest", spec.model_manifest_sha256.as_bytes());
    builder.add_bytes("material", spec.material.sha256().as_bytes());
    builder.add_u64("span.start", spec.start_frame);
    builder.add_u64("span.count", spec.frame_count);
    builder.add_u64("source.rate", u64::from(spec.sample_rate_hz));
    builder.add_u64("source.channels", u64::from(spec.channels));
    builder.add_u64("channels.count", spec.channel_selection.len() as u64);
    for (index, channel) in spec.channel_selection.iter().enumerate() {
        builder.add_u64(&format!("channels.{index}"), u64::from(*channel));
    }
    builder.add_optional_str("prompt", spec.prompt.as_deref());
    add_hashes(&mut builder, "references", &spec.reference_sha256);
    add_hashes(&mut builder, "masks", &spec.mask_sha256);
    builder.add_u64("parameters.count", spec.parameters.len() as u64);
    for (index, (name, value)) in spec.parameters.iter().enumerate() {
        let prefix = format!("parameters.{index}");
        builder.add_str(&format!("{prefix}.name"), name);
        add_wire_parameter(&mut builder, &prefix, value);
    }
    builder.add_str("runtime.worker", &spec.runtime.worker_name);
    builder.add_str("runtime.version", &spec.runtime.runtime);
    builder.add_optional_str("runtime.adapter", spec.runtime.adapter_sha256.as_deref());
    add_additivity(&mut builder, "bundle.additivity", &spec.additivity);
    builder.add_u64("outputs.count", spec.outputs.len() as u64);
    for (index, output) in spec.outputs.iter().enumerate() {
        let prefix = format!("outputs.{index}");
        builder.add_str(&format!("{prefix}.name"), &output.name);
        builder.add_str(&format!("{prefix}.path"), &output.relative_path);
        add_artifact_kind(&mut builder, &format!("{prefix}.kind"), &output.kind);
        builder.add_str(&format!("{prefix}.media"), &output.media_type);
        builder.add_u64(
            &format!("{prefix}.schema"),
            u64::from(output.schema_revision),
        );
        builder.add_u64(
            &format!("{prefix}.time_base"),
            u64::from(output.time_base_hz.unwrap_or(0)),
        );
        add_additivity(
            &mut builder,
            &format!("{prefix}.additivity"),
            &output.additivity,
        );
        builder.add_u64(&format!("{prefix}.labels"), output.labels.len() as u64);
        for (label_index, label) in output.labels.iter().enumerate() {
            let label_prefix = format!("{prefix}.labels.{label_index}");
            builder.add_str(&format!("{label_prefix}.name"), &label.model_authored_label);
            builder.add_str(&format!("{label_prefix}.ontology"), &label.ontology);
            builder.add_u64(
                &format!("{label_prefix}.revision"),
                u64::from(label.ontology_revision),
            );
            builder.add_str(
                &format!("{label_prefix}.confidence_kind"),
                confidence_kind_name(label.confidence_kind),
            );
            builder.add_optional_str(
                &format!("{label_prefix}.confidence"),
                label
                    .confidence
                    .map(|value| if value == 0.0 { 0_u32 } else { value.to_bits() }.to_string())
                    .as_deref(),
            );
        }
    }
    builder.add_u64("measurements.count", spec.measurement_names.len() as u64);
    for (index, name) in spec.measurement_names.iter().enumerate() {
        builder.add_str(&format!("measurements.{index}"), name);
    }
    builder.finish().digest()
}

fn add_artifact_kind(builder: &mut CacheKeyBuilder, name: &str, kind: &ArtifactKind) {
    match kind {
        ArtifactKind::Audio => builder.add_str(name, "audio"),
        ArtifactKind::Mask => builder.add_str(name, "mask"),
        ArtifactKind::EventMap => builder.add_str(name, "event-map"),
        ArtifactKind::Midi => builder.add_str(name, "midi"),
        ArtifactKind::Preset => builder.add_str(name, "preset"),
        ArtifactKind::ControlCurve => builder.add_str(name, "control-curve"),
        ArtifactKind::Embedding => builder.add_str(name, "embedding"),
        ArtifactKind::Measurement => builder.add_str(name, "measurement"),
        ArtifactKind::AdapterMetadata => builder.add_str(name, "adapter-metadata"),
        ArtifactKind::Custom(value) => {
            builder.add_str(name, "custom");
            builder.add_str(&format!("{name}.custom"), value);
        }
    }
}

fn add_additivity(builder: &mut CacheKeyBuilder, name: &str, additivity: &AdditivityDeclaration) {
    match additivity {
        AdditivityDeclaration::LinearSum => builder.add_str(name, "linear-sum"),
        AdditivityDeclaration::LinearSumWithResidual { residual_artifact } => {
            builder.add_str(name, "linear-sum-with-residual");
            builder.add_str(&format!("{name}.residual"), residual_artifact);
        }
        AdditivityDeclaration::OverlappingEstimates => {
            builder.add_str(name, "overlapping-estimates")
        }
        AdditivityDeclaration::Generative => builder.add_str(name, "generative"),
        AdditivityDeclaration::NonAudio => builder.add_str(name, "non-audio"),
    }
}

fn confidence_kind_name(kind: crate::model_claim::ClaimConfidenceKind) -> &'static str {
    use crate::model_claim::ClaimConfidenceKind;
    match kind {
        ClaimConfidenceKind::RelativeSupport => "relative-support",
        ClaimConfidenceKind::CalibratedProbability => "calibrated-probability",
        ClaimConfidenceKind::Logit => "logit",
        ClaimConfidenceKind::RankingScore => "ranking-score",
        ClaimConfidenceKind::Unavailable => "unavailable",
    }
}

fn add_hashes(builder: &mut CacheKeyBuilder, prefix: &str, values: &[ContentHash]) {
    builder.add_u64(&format!("{prefix}.count"), values.len() as u64);
    for (index, value) in values.iter().enumerate() {
        builder.add_bytes(&format!("{prefix}.{index}"), value.as_bytes());
    }
}

fn add_wire_parameter(builder: &mut CacheKeyBuilder, prefix: &str, value: &WireParameter) {
    match value {
        WireParameter::String(value) => {
            builder.add_str(&format!("{prefix}.kind"), "string");
            builder.add_str(&format!("{prefix}.value"), value);
        }
        WireParameter::Signed(value) => {
            builder.add_str(&format!("{prefix}.kind"), "signed");
            builder.add_i64(&format!("{prefix}.value"), *value);
        }
        WireParameter::Unsigned(value) => {
            builder.add_str(&format!("{prefix}.kind"), "unsigned");
            builder.add_u64(&format!("{prefix}.value"), *value);
        }
        WireParameter::Bool(value) => {
            builder.add_str(&format!("{prefix}.kind"), "bool");
            builder.add_bool(&format!("{prefix}.value"), *value);
        }
        WireParameter::Float(value) => {
            builder.add_str(&format!("{prefix}.kind"), "float");
            let bits = if *value == 0.0 { 0 } else { value.to_bits() };
            builder.add_u64(&format!("{prefix}.value"), bits);
        }
    }
}

fn validate_descriptor(
    index: usize,
    descriptor: &ArtifactDescriptor,
    expected: &ExpectedInferenceOutput,
) -> Result<(), RecipeError> {
    if descriptor.kind != expected.kind
        || descriptor.media_type != expected.media_type
        || descriptor.schema_revision != expected.schema_revision
        || descriptor.time_base_hz != expected.time_base_hz
        || descriptor.additivity != expected.additivity
    {
        return Err(RecipeError::OutputShape {
            index,
            name: expected.name.clone(),
        });
    }
    Ok(())
}

fn validate_backlinks(
    recipe: &InferenceRecipe,
    descriptor: &ArtifactDescriptor,
) -> Result<(), RecipeError> {
    let recipe_end = recipe.spec.start_frame + recipe.spec.frame_count;
    let material = recipe.spec.material.sha256().to_string();
    for backlink in &descriptor.source_backlinks {
        let Some(backlink_end) = backlink.start_frame.checked_add(backlink.frame_count) else {
            return Err(RecipeError::SourceBacklink(
                descriptor.relative_path.clone(),
            ));
        };
        if backlink.material_sha256 != material
            || backlink.frame_count == 0
            || backlink.start_frame < recipe.spec.start_frame
            || backlink_end > recipe_end
        {
            return Err(RecipeError::SourceBacklink(
                descriptor.relative_path.clone(),
            ));
        }
    }
    Ok(())
}

fn verify_stored_bytes(root: &Path, descriptor: &ArtifactDescriptor) -> Result<(), RecipeError> {
    let relative = Path::new(&descriptor.relative_path);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RecipeError::StoredArtifact(
            descriptor.relative_path.clone(),
        ));
    }
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| RecipeError::Io {
        path: path.clone(),
        error,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(RecipeError::StoredArtifact(
            descriptor.relative_path.clone(),
        ));
    }
    let bytes = fs::read(&path).map_err(|error| RecipeError::Io {
        path: path.clone(),
        error,
    })?;
    if bytes.len() as u64 != descriptor.byte_len
        || sha256_bytes(&bytes).to_string() != descriptor.sha256
    {
        return Err(RecipeError::StoredArtifact(
            descriptor.relative_path.clone(),
        ));
    }
    Ok(())
}

fn validate_name(_field: &str, value: &str) -> Result<(), RecipeError> {
    if value.is_empty()
        || value.len() > 160
        || value.contains(char::is_whitespace)
        || value.contains(['/', '\\', '\0'])
    {
        return Err(RecipeError::Invalid(
            "names must be compact non-path labels",
        ));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), RecipeError> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RecipeError::Invalid(
            "output paths must be normalized relative paths",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub enum RecipeError {
    Invalid(&'static str),
    MaterialDigestMismatch {
        expected: ContentHash,
        actual: ContentHash,
    },
    StoredRecipeMismatch,
    OutputCount {
        expected: usize,
        actual: usize,
    },
    OutputShape {
        index: usize,
        name: String,
    },
    MissingOutput {
        name: String,
        relative_path: String,
    },
    MeasurementShape,
    SourceBacklink(String),
    StoredArtifact(String),
    Claim(String),
    Io {
        path: std::path::PathBuf,
        error: std::io::Error,
    },
}

impl fmt::Display for RecipeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(detail) => formatter.write_str(detail),
            Self::MaterialDigestMismatch { expected, actual } => {
                write!(
                    formatter,
                    "material digest mismatch: expected {expected}, got {actual}"
                )
            }
            Self::StoredRecipeMismatch => {
                formatter.write_str("stored result belongs to another recipe")
            }
            Self::OutputCount { expected, actual } => {
                write!(formatter, "expected {expected} outputs, got {actual}")
            }
            Self::OutputShape { index, name } => {
                write!(
                    formatter,
                    "stored output {index} does not match `{name}` semantics"
                )
            }
            Self::MissingOutput {
                name,
                relative_path,
            } => write!(
                formatter,
                "stored bundle is missing `{name}` at `{relative_path}`"
            ),
            Self::MeasurementShape => {
                formatter.write_str("stored measurements do not match the recipe")
            }
            Self::SourceBacklink(path) => {
                write!(
                    formatter,
                    "stored artifact has an alien source backlink: {path}"
                )
            }
            Self::StoredArtifact(path) => write!(
                formatter,
                "stored artifact bytes failed verification: {path}"
            ),
            Self::Claim(detail) => write!(formatter, "could not construct model claim: {detail}"),
            Self::Io { path, error } => {
                write!(formatter, "could not read {}: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for RecipeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_claim::ClaimConfidenceKind;
    use crate::model_wire::{MeasurementValue, WorkerResult};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn hash(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    fn output(name: &str) -> ExpectedInferenceOutput {
        ExpectedInferenceOutput {
            name: name.into(),
            relative_path: format!("{name}.f32"),
            kind: ArtifactKind::Audio,
            media_type: "audio/f32le".into(),
            schema_revision: 1,
            time_base_hz: Some(48_000),
            additivity: AdditivityDeclaration::OverlappingEstimates,
            labels: vec![ModelLabel {
                model_authored_label: name.into(),
                ontology: "fixture-vocabulary".into(),
                ontology_revision: 1,
                confidence_kind: ClaimConfidenceKind::Unavailable,
                confidence: None,
            }],
        }
    }

    fn recipe() -> InferenceRecipe {
        InferenceRecipe::new(InferenceRecipeSpec {
            model_manifest_sha256: hash(7),
            material: VerifiedMaterial::from_bytes(Arc::<[u8]>::from(&b"pcm"[..])),
            start_frame: 12,
            frame_count: 24,
            sample_rate_hz: 48_000,
            channels: 2,
            channel_selection: vec![1, 0],
            prompt: Some("drums".into()),
            reference_sha256: vec![],
            mask_sha256: vec![],
            parameters: BTreeMap::from([("quality".into(), WireParameter::Unsigned(2))]),
            runtime: WorkerRuntimeProvenance {
                worker_name: "fixture-worker".into(),
                runtime: "fixture-runtime-v1".into(),
                adapter_sha256: Some(hash(8).to_string()),
            },
            additivity: AdditivityDeclaration::OverlappingEstimates,
            outputs: vec![output("drums")],
            measurement_names: BTreeSet::from(["support".into()]),
        })
        .unwrap()
    }

    fn descriptor(name: &str, bytes: &[u8]) -> ArtifactDescriptor {
        ArtifactDescriptor {
            relative_path: format!("{name}.f32"),
            sha256: sha256_bytes(bytes).to_string(),
            byte_len: bytes.len() as u64,
            kind: ArtifactKind::Audio,
            media_type: "audio/f32le".into(),
            schema_revision: 1,
            time_base_hz: Some(48_000),
            additivity: AdditivityDeclaration::OverlappingEstimates,
            source_backlinks: vec![],
        }
    }

    #[test]
    fn material_digest_is_computed_and_claimed_digest_is_checked() {
        let material = VerifiedMaterial::from_bytes(Arc::<[u8]>::from(&b"abc"[..]));
        assert_eq!(
            material.sha256().to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(
            VerifiedMaterial::from_claimed_bytes(Arc::<[u8]>::from(&b"abc"[..]), hash(4)).is_err()
        );
    }

    #[test]
    fn recipe_identity_is_canonical_and_semantic() {
        let first = recipe();
        let second = recipe();
        assert_eq!(first.id(), second.id());
        assert_eq!(first.spec.channel_selection, vec![1, 0]);

        let mut changed_spec = first.spec().clone();
        changed_spec.outputs[0].name = "percussion".into();
        let changed = InferenceRecipe::new(changed_spec).unwrap();
        assert_ne!(first.id(), changed.id());
    }

    #[test]
    fn task_service_recipe_cannot_override_authoritative_semantics() {
        let recipe = recipe();
        let task = recipe.model_task_recipe("fixture-model");
        assert_eq!(task.model_id, "fixture-model");
        assert_eq!(task.cache_key, recipe.id().to_string());
        assert_eq!(task.material.bytes, recipe.material().bytes());
        assert_eq!(task.material.sha256, recipe.material().sha256().to_string());
        assert_eq!(
            task.publication.model_manifest_sha256,
            recipe.spec().model_manifest_sha256.to_string()
        );
        assert_eq!(
            task.publication.source.start_frame,
            recipe.spec().start_frame
        );
        assert_eq!(task.publication.additivity, recipe.spec().additivity);
        assert_eq!(task.publication.outputs.len(), recipe.spec().outputs.len());
        assert_eq!(task.publication.outputs[0].0, recipe.spec().outputs[0].name);
    }

    #[test]
    fn stored_bytes_and_semantics_authorize_claim_construction() {
        let recipe = recipe();
        let directory = temp_directory("valid");
        fs::write(directory.join("drums.f32"), b"stem").unwrap();
        let descriptor = ArtifactDescriptor {
            relative_path: "drums.f32".into(),
            sha256: sha256_bytes(b"stem").to_string(),
            byte_len: 4,
            kind: ArtifactKind::Audio,
            media_type: "audio/f32le".into(),
            schema_revision: 1,
            time_base_hz: Some(48_000),
            additivity: AdditivityDeclaration::OverlappingEstimates,
            source_backlinks: vec![],
        };
        let stored = StoredResult {
            directory: directory.clone(),
            result: WorkerResult {
                job_id: "job-1".into(),
                cache_key: recipe.id().to_string(),
                artifacts: vec![descriptor],
                measurements: vec![MeasurementValue {
                    name: "support".into(),
                    value: WireParameter::Float(0.8),
                }],
            },
        };
        let bundle = recipe.validate_stored(stored).unwrap();
        assert_eq!(bundle.claim.id.as_str(), recipe.id().to_string());
        assert_eq!(bundle.claim.artifacts[0].output_name, "drums");
        assert_eq!(
            bundle.claim.source.material_sha256,
            recipe.material().sha256().to_string()
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stored_result_cannot_relabel_or_hide_tampered_bytes() {
        let recipe = recipe();
        let directory = temp_directory("tampered");
        fs::write(directory.join("drums.f32"), b"evil").unwrap();
        let descriptor = ArtifactDescriptor {
            relative_path: "drums.f32".into(),
            sha256: sha256_bytes(b"stem").to_string(),
            byte_len: 4,
            kind: ArtifactKind::Audio,
            media_type: "audio/f32le".into(),
            schema_revision: 1,
            time_base_hz: Some(48_000),
            additivity: AdditivityDeclaration::OverlappingEstimates,
            source_backlinks: vec![],
        };
        let stored = StoredResult {
            directory: directory.clone(),
            result: WorkerResult {
                job_id: "job-2".into(),
                cache_key: recipe.id().to_string(),
                artifacts: vec![descriptor],
                measurements: vec![MeasurementValue {
                    name: "support".into(),
                    value: WireParameter::Float(0.8),
                }],
            },
        };
        assert!(matches!(
            recipe.validate_stored(stored),
            Err(RecipeError::StoredArtifact(_))
        ));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stored_outputs_join_by_recipe_path_not_worker_order() {
        let base = recipe();
        let mut spec = base.spec().clone();
        spec.outputs.push(output("bass"));
        let recipe = InferenceRecipe::new(spec).unwrap();
        let mut reordered = recipe.spec().clone();
        reordered.outputs.reverse();
        assert_eq!(recipe.id(), InferenceRecipe::new(reordered).unwrap().id());
        let directory = temp_directory("permuted");
        fs::write(directory.join("drums.f32"), b"drum").unwrap();
        fs::write(directory.join("bass.f32"), b"bass").unwrap();
        let stored = StoredResult {
            directory: directory.clone(),
            result: WorkerResult {
                job_id: "job-permuted".into(),
                cache_key: recipe.id().to_string(),
                artifacts: vec![descriptor("bass", b"bass"), descriptor("drums", b"drum")],
                measurements: vec![MeasurementValue {
                    name: "support".into(),
                    value: WireParameter::Float(0.8),
                }],
            },
        };
        let bundle = recipe.validate_stored(stored).unwrap();
        let names: Vec<_> = bundle
            .claim
            .artifacts
            .iter()
            .map(|artifact| artifact.output_name.as_str())
            .collect();
        assert_eq!(names, vec!["bass", "drums"]);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn additivity_and_backlinks_cannot_change_bundle_meaning() {
        let base = recipe();
        let mut incoherent = base.spec().clone();
        incoherent.additivity = AdditivityDeclaration::LinearSumWithResidual {
            residual_artifact: "missing".into(),
        };
        assert!(InferenceRecipe::new(incoherent).is_err());

        let directory = temp_directory("alien-backlink");
        fs::write(directory.join("drums.f32"), b"stem").unwrap();
        let mut artifact = descriptor("drums", b"stem");
        artifact
            .source_backlinks
            .push(crate::model_wire::SourceBacklink {
                material_sha256: hash(99).to_string(),
                start_frame: 12,
                frame_count: 24,
            });
        let stored = StoredResult {
            directory: directory.clone(),
            result: WorkerResult {
                job_id: "job-alien".into(),
                cache_key: base.id().to_string(),
                artifacts: vec![artifact],
                measurements: vec![MeasurementValue {
                    name: "support".into(),
                    value: WireParameter::Float(0.8),
                }],
            },
        };
        assert!(matches!(
            base.validate_stored(stored),
            Err(RecipeError::SourceBacklink(_))
        ));
        let _ = fs::remove_dir_all(directory);
    }

    fn temp_directory(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "audec-inference-recipe-{}-{label}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}
