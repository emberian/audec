//! Pinned Beat This small0 adapter contract.
//!
//! This module names an optional, CPU-only analysis worker; it does not
//! download a checkpoint, import Python, or promote its output over Audec's
//! native rhythm evidence.  The worker first publishes frame logits as raw
//! measurement artifacts and only then publishes derived beat/downbeat events.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use crate::model_claim::{
    ClaimConfidenceKind, ClaimError, ClaimSource, ModelClaimBundle, ModelLabel,
    WorkerRuntimeProvenance,
};
use crate::model_registry::{
    ArtifactLock, ArtifactManifest, ArtifactRole, DownloadState, ModelCapability,
    ModelRegistration, RegistryError, RuntimeDescriptor, WorkerDescriptor,
};
use crate::model_task_service::{ModelTaskRecipe, TaskMaterial};
#[cfg(test)]
use crate::model_wire::ArtifactKind;
use crate::model_wire::{AdditivityDeclaration, WireParameter};
use crate::model_worker::{
    Architecture, AtomicStaging, AudioContract, Backend, ChannelContract, ContentHash,
    EffectiveParameter, ExactRevision, ExecutionContract, LicenseProvenance, LicenseReference,
    MaterialSpan, ModelArtifacts, ModelManifest, Normalization, NumericPrecision, OutputAdditivity,
    OutputContract, ParameterValue, Redistribution, SampleEncoding, SeparationRequest,
    TrainingProvenance, MANIFEST_SCHEMA_VERSION, PROTOCOL_VERSION,
};
use crate::worker_runtime::ClaimPublication;

pub const MODEL_ID: &str = "beat-this-small0-1.1.0";
pub const WORKER_NAME: &str = "audec-beat-this-worker";
pub const RUNTIME_ID: &str = "beat-this-python-cpu";
pub const INSTALL_DIRECTORY: &str = "beat-this-small0-1.1.0";
pub const CHECKPOINT_FILE: &str = "small0.ckpt";
pub const CHECKPOINT_SHA256: &str =
    "6074be2c4d490c5f6101fcc374a1ec72ae93456e23bb6019783b849f5dc7d47b";
pub const CHECKPOINT_URL: &str =
    "https://cloud.cp.jku.at/public.php/dav/files/7ik4RrBKTS273gp/small0.ckpt";
pub const INPUT_SAMPLE_RATE_HZ: u32 = 22_050;
pub const CHUNK_FRAMES: u64 = 661_500;
pub const OVERLAP_FRAMES: u64 = 5_292;
pub const ADAPTER_PROTOCOL: &str = "beat-this-jsonl-v1";

/// Instructions shown by an installer UI or CLI. Download remains a person
/// initiated action; `ModelRegistry::verify` is the only acceptance step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeatThisInstallManifest {
    pub model_id: &'static str,
    pub source_url: &'static str,
    pub destination: PathBuf,
    pub sha256: &'static str,
    pub python_requirement: &'static str,
    pub runtime_requirements: &'static [&'static str],
}

pub fn install_manifest() -> BeatThisInstallManifest {
    BeatThisInstallManifest {
        model_id: MODEL_ID,
        source_url: CHECKPOINT_URL,
        destination: PathBuf::from(INSTALL_DIRECTORY).join(CHECKPOINT_FILE),
        sha256: CHECKPOINT_SHA256,
        python_requirement: "beat-this==1.1.0",
        runtime_requirements: &[
            "torch>=2.0 (CPU build)",
            "tqdm",
            "einops",
            "soxr",
            "rotary-embedding-torch",
        ],
    }
}

/// Exact immutable registry entry from `docs/ML_MODELS.md`.
///
/// The Configuration and Weights locks intentionally name one Lightning
/// checkpoint: upstream embeds small0 hyperparameters in that checkpoint.
pub fn small0_registration() -> Result<ModelRegistration, BeatThisError> {
    let checkpoint = hash(CHECKPOINT_SHA256)?;
    let license = LicenseProvenance {
        code: LicenseReference::Spdx("MIT".into()),
        checkpoint: LicenseReference::Spdx("MIT".into()),
        redistribution: Redistribution::RequiresReview,
        source_url: Some(CHECKPOINT_URL.into()),
        review_notes: "Authors explicitly note copyrighted and limited-CC training files; checkpoint embeds its hyperparameters.".into(),
    };
    let registration = ModelRegistration {
        manifest: ModelManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            model_id: MODEL_ID.into(),
            architecture: Architecture {
                family: "beat-this".into(),
                version: "small0@b95c8ab0c58c2d9fcfd40508ae8dffbc05ac4f5c".into(),
            },
            revision: ExactRevision::Release {
                version: "pypi-1.1.0+small0".into(),
                source_hash: hash("3017c741f972972a650edcaccfe5760687fe4f5587feaa98896d90f866c2435c")?,
            },
            artifacts: ModelArtifacts {
                weights_sha256: checkpoint,
                config_sha256: checkpoint,
                adapter_sha256: None,
                conversion_recipe_sha256: None,
                numerical_validation_sha256: None,
            },
            license: license.clone(),
            training: TrainingProvenance {
                summary: "mixed public/copyrighted beat datasets documented by Beat This!; exact annotations release v1.0".into(),
                sources: Vec::new(),
                documentation_sha256: hash("be7b51a9b6ff2041fdde81061d079c29451583f2c06c9733bece9282cc7afab0")?,
            },
            input: AudioContract {
                sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
                channels: ChannelContract::Mono,
                encoding: SampleEncoding::Float32Le,
            },
            execution: ExecutionContract {
                chunk_frames: CHUNK_FRAMES,
                overlap_frames: OVERLAP_FRAMES,
                normalization: Normalization::None,
                backend: Backend::Cpu {
                    runtime: "beat-this==1.1.0/torch-cpu".into(),
                    precision: NumericPrecision::Float32,
                },
                estimated_peak_memory_bytes: 536_870_912,
                required_accelerators: Vec::new(),
            },
            output: OutputContract {
                names: output_names().into_iter().map(str::to_owned).collect(),
                sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
                channels: ChannelContract::Mono,
                additivity: OutputAdditivity::NonAudio {
                    units: "frame probabilities and sample-referenced events".into(),
                },
            },
            golden_validations: Vec::new(),
        },
        artifacts: ArtifactManifest {
            install_directory: INSTALL_DIRECTORY.into(),
            artifacts: vec![
                ArtifactLock {
                    role: ArtifactRole::Weights,
                    relative_path: CHECKPOINT_FILE.into(),
                    sha256: checkpoint,
                    byte_len: None,
                    required: true,
                },
                ArtifactLock {
                    role: ArtifactRole::Configuration,
                    relative_path: CHECKPOINT_FILE.into(),
                    sha256: checkpoint,
                    byte_len: None,
                    required: true,
                },
            ],
        },
        license,
        capabilities: BTreeSet::from([ModelCapability::BeatAndDownbeat]),
        selection_priority: 10,
        workers: vec![WorkerDescriptor {
            worker_name: WORKER_NAME.into(),
            runtime: RuntimeDescriptor {
                runtime_id: RUNTIME_ID.into(),
                protocol_version: PROTOCOL_VERSION,
                supported_backends: BTreeSet::from(["cpu".into()]),
                required_accelerators: BTreeSet::new(),
            },
        }],
        download_state: DownloadState::UserDownloadRequired,
    };
    registration.validate()?;
    Ok(registration)
}

/// The decoded model material must already be mono Float32LE at 22.05 kHz.
/// Resampling/downmixing belongs to a separately recorded transform; this
/// adapter will not hide it as an inference-side convenience operation.
pub fn task_recipe(material: TaskMaterial) -> Result<ModelTaskRecipe, BeatThisError> {
    validate_material(&material)?;
    let registration = small0_registration()?;
    let manifest_hash = registration.manifest.canonical_hash()?;
    let material_hash = ContentHash::from_str(&material.sha256)
        .map_err(|error| BeatThisError::InvalidInput(error.to_string()))?;
    let parameters = effective_parameters();
    let cache_request = SeparationRequest {
        id: crate::model_worker::JobId("beat-this-cache-identity".into()),
        model_manifest_sha256: manifest_hash,
        material_sha256: material_hash,
        span: MaterialSpan {
            start_frame: material.start_frame,
            frame_count: material.frame_count,
        },
        channel_selection: Vec::new(),
        prompt: None,
        reference_sha256: Vec::new(),
        mask_sha256: Vec::new(),
        parameters: parameters.clone(),
        // Cache identity deliberately excludes publication paths; these only
        // satisfy the generic request's validation contract here.
        staging: AtomicStaging {
            staging_path: PathBuf::from("beat-this-staging"),
            destination_path: PathBuf::from("beat-this-result"),
        },
    };
    let cache_key = cache_request.cache_key(&registration.manifest)?.to_string();
    let source = ClaimSource {
        material_sha256: material.sha256.clone(),
        start_frame: material.start_frame,
        frame_count: material.frame_count,
        sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
        channels: 1,
    };
    let publication = ClaimPublication {
        model_manifest_sha256: manifest_hash.to_string(),
        source,
        runtime: WorkerRuntimeProvenance {
            worker_name: WORKER_NAME.into(),
            runtime: "beat-this-1.1.0-torch-cpu".into(),
            adapter_sha256: None,
        },
        additivity: AdditivityDeclaration::NonAudio,
        outputs: output_labels(),
    };
    Ok(ModelTaskRecipe {
        model_id: MODEL_ID.into(),
        cache_key,
        material,
        prompt: None,
        reference_sha256: Vec::new(),
        mask_sha256: Vec::new(),
        parameters: wire_parameters(),
        publication,
    })
}

/// A portable interpretation layer over an immutable worker claim. These are
/// *competing* rhythm observations, intended to sit beside native `rhythm`
/// hypotheses until a person chooses how (or whether) to use them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeatThisRhythmEvidence {
    pub claim_id: String,
    pub beat_logits: String,
    pub downbeat_logits: String,
    pub tempo: BeatThisCandidate,
    pub beat_grid: BeatThisCandidate,
    pub downbeat_grid: BeatThisCandidate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatThisCandidateKind {
    Tempo,
    BeatGrid,
    DownbeatGrid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeatThisCandidate {
    pub kind: BeatThisCandidateKind,
    /// This is an artifact locator, not a source identity or acceptance.
    pub supporting_events: String,
    pub relationship_to_native_rhythm: RhythmRelationship,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RhythmRelationship {
    CompetingEvidence,
}

impl BeatThisRhythmEvidence {
    pub fn from_claim(claim: &ModelClaimBundle) -> Result<Self, BeatThisError> {
        let beat_logits = locator(claim, "beat-logits")?;
        let downbeat_logits = locator(claim, "downbeat-logits")?;
        let beat_events = locator(claim, "beat-events")?;
        let downbeat_events = locator(claim, "downbeat-events")?;
        Ok(Self {
            claim_id: claim.id.as_str().into(),
            beat_logits,
            downbeat_logits,
            tempo: BeatThisCandidate {
                kind: BeatThisCandidateKind::Tempo,
                supporting_events: beat_events.clone(),
                relationship_to_native_rhythm: RhythmRelationship::CompetingEvidence,
            },
            beat_grid: BeatThisCandidate {
                kind: BeatThisCandidateKind::BeatGrid,
                supporting_events: beat_events,
                relationship_to_native_rhythm: RhythmRelationship::CompetingEvidence,
            },
            downbeat_grid: BeatThisCandidate {
                kind: BeatThisCandidateKind::DownbeatGrid,
                supporting_events: downbeat_events,
                relationship_to_native_rhythm: RhythmRelationship::CompetingEvidence,
            },
        })
    }
}

fn locator(claim: &ModelClaimBundle, output: &str) -> Result<String, BeatThisError> {
    claim
        .analysis_locator(output)
        .ok_or_else(|| BeatThisError::MissingOutput(output.into()))
}

fn output_names() -> [&'static str; 4] {
    [
        "beat-logits",
        "downbeat-logits",
        "beat-events",
        "downbeat-events",
    ]
}

fn output_labels() -> Vec<(String, Vec<ModelLabel>)> {
    vec![
        (
            "beat-logits".into(),
            vec![label("beat-logit", ClaimConfidenceKind::Logit)],
        ),
        (
            "downbeat-logits".into(),
            vec![label("downbeat-logit", ClaimConfidenceKind::Logit)],
        ),
        (
            "beat-events".into(),
            vec![label("beat-event", ClaimConfidenceKind::Unavailable)],
        ),
        (
            "downbeat-events".into(),
            vec![label("downbeat-event", ClaimConfidenceKind::Unavailable)],
        ),
    ]
}

fn label(name: &str, confidence_kind: ClaimConfidenceKind) -> ModelLabel {
    ModelLabel {
        model_authored_label: name.into(),
        ontology: "beat-this".into(),
        ontology_revision: 1,
        confidence_kind,
        confidence: None,
    }
}

fn effective_parameters() -> Vec<EffectiveParameter> {
    vec![
        parameter("adapter", ParameterValue::String(ADAPTER_PROTOCOL.into())),
        parameter("chunk_frames", ParameterValue::Unsigned(CHUNK_FRAMES)),
        parameter("input_encoding", ParameterValue::String("float32le".into())),
        parameter(
            "input_rate_hz",
            ParameterValue::Unsigned(u64::from(INPUT_SAMPLE_RATE_HZ)),
        ),
        parameter("overlap_frames", ParameterValue::Unsigned(OVERLAP_FRAMES)),
        parameter("postprocess", ParameterValue::String("minimal-v1".into())),
    ]
}

fn parameter(name: &str, value: ParameterValue) -> EffectiveParameter {
    EffectiveParameter {
        name: name.into(),
        value,
    }
}

fn wire_parameters() -> BTreeMap<String, WireParameter> {
    BTreeMap::from([
        (
            "adapter".into(),
            WireParameter::String(ADAPTER_PROTOCOL.into()),
        ),
        ("chunk_frames".into(), WireParameter::Unsigned(CHUNK_FRAMES)),
        (
            "input_encoding".into(),
            WireParameter::String("float32le".into()),
        ),
        (
            "input_rate_hz".into(),
            WireParameter::Unsigned(u64::from(INPUT_SAMPLE_RATE_HZ)),
        ),
        (
            "overlap_frames".into(),
            WireParameter::Unsigned(OVERLAP_FRAMES),
        ),
        (
            "postprocess".into(),
            WireParameter::String("minimal-v1".into()),
        ),
    ])
}

fn validate_material(material: &TaskMaterial) -> Result<(), BeatThisError> {
    if material.frame_count == 0 {
        return Err(BeatThisError::InvalidInput(
            "Beat This requires a non-empty mono material span".into(),
        ));
    }
    if !material.channel_selection.is_empty() {
        return Err(BeatThisError::InvalidInput(
            "Beat This input has already been downmixed to mono; channel selection is not valid"
                .into(),
        ));
    }
    let expected_bytes = material.frame_count.checked_mul(4).ok_or_else(|| {
        BeatThisError::InvalidInput("material frame count overflows byte count".into())
    })?;
    if usize::try_from(expected_bytes).ok() != Some(material.bytes.len()) {
        return Err(BeatThisError::InvalidInput(format!(
            "Beat This expects {expected_bytes} Float32LE bytes, got {}",
            material.bytes.len()
        )));
    }
    ContentHash::from_str(&material.sha256)
        .map_err(|error| BeatThisError::InvalidInput(error.to_string()))?;
    Ok(())
}

fn hash(value: &str) -> Result<ContentHash, BeatThisError> {
    ContentHash::from_str(value).map_err(|error| BeatThisError::InvalidManifest(error.to_string()))
}

#[derive(Debug)]
pub enum BeatThisError {
    InvalidManifest(String),
    InvalidInput(String),
    Registry(RegistryError),
    Worker(crate::model_worker::ValidationError),
    Claim(ClaimError),
    MissingOutput(String),
}

impl std::fmt::Display for BeatThisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidManifest(error) | Self::InvalidInput(error) => f.write_str(error),
            Self::Registry(error) => error.fmt(f),
            Self::Worker(error) => error.fmt(f),
            Self::Claim(error) => error.fmt(f),
            Self::MissingOutput(output) => {
                write!(f, "Beat This result lacks required {output} output")
            }
        }
    }
}
impl std::error::Error for BeatThisError {}
impl From<RegistryError> for BeatThisError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}
impl From<crate::model_worker::ValidationError> for BeatThisError {
    fn from(value: crate::model_worker::ValidationError) -> Self {
        Self::Worker(value)
    }
}
impl From<ClaimError> for BeatThisError {
    fn from(value: ClaimError) -> Self {
        Self::Claim(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_claim::{ModelClaimArtifact, ModelClaimBundle};
    use crate::model_wire::{ArtifactDescriptor, SourceBacklink};

    fn material() -> TaskMaterial {
        TaskMaterial {
            sha256: "01".repeat(32),
            bytes: vec![0; 16],
            start_frame: 320,
            frame_count: 4,
            channel_selection: Vec::new(),
        }
    }

    #[test]
    fn registration_locks_the_embedded_configuration_to_the_same_checkpoint() {
        let registration = small0_registration().unwrap();
        registration.validate().unwrap();
        assert_eq!(registration.artifacts.artifacts.len(), 2);
        assert_eq!(
            registration.artifacts.artifacts[0].relative_path,
            registration.artifacts.artifacts[1].relative_path
        );
        assert_eq!(
            registration.artifacts.artifacts[0].sha256,
            registration.artifacts.artifacts[1].sha256
        );
        assert_eq!(
            install_manifest().destination,
            PathBuf::from(INSTALL_DIRECTORY).join(CHECKPOINT_FILE)
        );
    }

    #[test]
    fn task_recipe_keeps_no_hidden_channel_selection_and_keys_postprocessing() {
        let recipe = task_recipe(material()).unwrap();
        assert_eq!(recipe.model_id, MODEL_ID);
        assert_eq!(
            recipe.parameters.get("postprocess"),
            Some(&WireParameter::String("minimal-v1".into()))
        );
        assert_eq!(recipe.publication.outputs[0].0, "beat-logits");
        assert_eq!(recipe.publication.outputs[1].0, "downbeat-logits");
        assert!(matches!(
            recipe.publication.outputs[0].1[0].confidence_kind,
            ClaimConfidenceKind::Logit
        ));
    }

    #[test]
    fn evidence_never_discards_logits_or_promotes_a_native_rhythm() {
        let recipe = task_recipe(material()).unwrap();
        let artifacts = output_names()
            .into_iter()
            .enumerate()
            .map(|(index, name)| ModelClaimArtifact {
                descriptor: ArtifactDescriptor {
                    relative_path: format!("{name}.json"),
                    sha256: format!("{:064x}", index + 1),
                    byte_len: 1,
                    kind: if name.ends_with("events") {
                        ArtifactKind::EventMap
                    } else {
                        ArtifactKind::Measurement
                    },
                    media_type: "application/json".into(),
                    schema_revision: 1,
                    time_base_hz: None,
                    additivity: AdditivityDeclaration::NonAudio,
                    source_backlinks: vec![SourceBacklink {
                        material_sha256: "01".repeat(32),
                        start_frame: 320,
                        frame_count: 4,
                    }],
                },
                output_name: name.into(),
                labels: output_labels()[index].1.clone(),
            })
            .collect();
        let claim = ModelClaimBundle::new(
            recipe.publication.model_manifest_sha256,
            recipe.cache_key,
            recipe.publication.source,
            recipe.publication.runtime,
            AdditivityDeclaration::NonAudio,
            artifacts,
        )
        .unwrap();
        let evidence = BeatThisRhythmEvidence::from_claim(&claim).unwrap();
        assert!(evidence.beat_logits.contains("beat-logits"));
        assert_eq!(
            evidence.tempo.relationship_to_native_rhythm,
            RhythmRelationship::CompetingEvidence
        );
    }
}
