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
    ArtifactLock, ArtifactManifest, ArtifactRole, DownloadState, InstallStatus, ModelCapability,
    ModelRegistration, ModelRegistry, RegistryError, RuntimeDescriptor, WorkerDescriptor,
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
use crate::worker_runtime::{ClaimPublication, WorkerLaunch};

pub const MODEL_ID: &str = "beat-this-rten-small-1.0.0";
pub const WORKER_NAME: &str = "audec-beat-this-worker";
pub const RUNTIME_ID: &str = "beat-this-rten-0.24";
pub const INSTALL_DIRECTORY: &str = "beat-this-rten-small-1.0.0";
pub const SOURCE_REVISION: &str = "089b509247e6fdcec666511c0dcf0d5f39c21e73";
pub const SOURCE_TREE_SHA256: &str =
    "1b82c99b959b4670d92421d098d592efcd98e18fcbbe4cdbffc5b128f4a48a4e";
pub const SOURCE_URL: &str = "https://github.com/danigb/beat-this-rs";
pub const MEL_MODEL_FILE: &str = "mel_spectrogram.onnx";
pub const MEL_MODEL_BYTES: u64 = 270_742;
pub const MEL_MODEL_SHA256: &str =
    "fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9";
pub const BEAT_MODEL_FILE: &str = "beat_this_small.onnx";
pub const BEAT_MODEL_BYTES: u64 = 10_555_592;
pub const BEAT_MODEL_SHA256: &str =
    "a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f";
pub const CONVERSION_RECIPE_SHA256: &str =
    "0b31944968c089a6f0b7869e9eb2c0a8af7b729f255fe8daf4646648baa8171d";
pub const NUMERICAL_VALIDATION_SHA256: &str =
    "fafae275a6df07d0c10f0a0f06622cfa075abe680d052d21337623b5639f7623";
pub const INPUT_SAMPLE_RATE_HZ: u32 = 22_050;
pub const CHUNK_FRAMES: u64 = 661_500;
pub const OVERLAP_FRAMES: u64 = 5_292;
pub const LOGIT_FRAME_RATE_HZ: u32 = 50;
pub const ADAPTER_PROTOCOL: &str = "beat-this-rten-jsonl-v1";

/// Instructions shown by an installer UI or CLI. Download remains a person
/// initiated action; `ModelRegistry::verify` is the only acceptance step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeatThisInstallManifest {
    pub model_id: &'static str,
    pub source_url: &'static str,
    pub source_revision: &'static str,
    pub destination_directory: PathBuf,
    pub artifacts: &'static [BeatThisInstallArtifact],
    pub runtime: &'static str,
    pub redistribution: Redistribution,
    pub review_notes: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeatThisInstallArtifact {
    pub repository_path: &'static str,
    pub destination_file: &'static str,
    pub byte_len: u64,
    pub sha256: &'static str,
}

const INSTALL_ARTIFACTS: &[BeatThisInstallArtifact] = &[
    BeatThisInstallArtifact {
        repository_path: "models/mel_spectrogram.onnx",
        destination_file: MEL_MODEL_FILE,
        byte_len: MEL_MODEL_BYTES,
        sha256: MEL_MODEL_SHA256,
    },
    BeatThisInstallArtifact {
        repository_path: "models/beat_this_small.onnx",
        destination_file: BEAT_MODEL_FILE,
        byte_len: BEAT_MODEL_BYTES,
        sha256: BEAT_MODEL_SHA256,
    },
    BeatThisInstallArtifact {
        repository_path: "git archive --format=tar 089b509247e6fdcec666511c0dcf0d5f39c21e73",
        destination_file: "beat-this-rs.tar",
        byte_len: 17_664_000,
        sha256: SOURCE_TREE_SHA256,
    },
    BeatThisInstallArtifact {
        repository_path: "scripts/ckpt2onnx.py",
        destination_file: "ckpt2onnx.py",
        byte_len: 3_440,
        sha256: CONVERSION_RECIPE_SHA256,
    },
    BeatThisInstallArtifact {
        repository_path: "tests/fixtures/golden_small.json",
        destination_file: "golden_small.json",
        byte_len: 8_436,
        sha256: NUMERICAL_VALIDATION_SHA256,
    },
];

pub fn install_manifest() -> BeatThisInstallManifest {
    BeatThisInstallManifest {
        model_id: MODEL_ID,
        source_url: SOURCE_URL,
        source_revision: SOURCE_REVISION,
        destination_directory: PathBuf::from(INSTALL_DIRECTORY),
        artifacts: INSTALL_ARTIFACTS,
        runtime: "beat-this-rs@089b509/rten==0.24.0/float32-cpu",
        redistribution: Redistribution::RequiresReview,
        review_notes: "Install the two exact committed ONNX files locally after reviewing the Beat This! training-corpus disclosure. Audec never downloads or bundles them automatically.",
    }
}

/// Runtime-ready state after the registry has independently authenticated all
/// five installation artifacts. An unavailable provider remains inspectable;
/// no caller needs to turn `Missing` or `Invalid` into a generic launch error.
#[derive(Clone, Debug)]
pub enum BeatThisProviderState {
    Installed {
        manifest_sha256: ContentHash,
        model_directory: PathBuf,
        launch: WorkerLaunch,
    },
    Unavailable(InstallStatus),
}

pub fn provider_state(
    registry: &ModelRegistry,
    worker_executable: impl Into<PathBuf>,
) -> Result<BeatThisProviderState, BeatThisError> {
    let registration = small0_registration()?;
    let status = registry.verify(&registration)?;
    let InstallStatus::Installed { manifest_sha256 } = status else {
        return Ok(BeatThisProviderState::Unavailable(status));
    };
    let model_directory = registry
        .root()
        .join(&registration.artifacts.install_directory)
        .canonicalize()
        .map_err(|error| BeatThisError::InvalidInput(error.to_string()))?;
    Ok(BeatThisProviderState::Installed {
        manifest_sha256,
        launch: WorkerLaunch {
            program: worker_executable.into(),
            arguments: vec![
                "--model-directory".into(),
                model_directory.to_string_lossy().into_owned(),
            ],
            expected_worker_name: WORKER_NAME.into(),
        },
        model_directory,
    })
}

/// Exact immutable registry entry for the audited pure-Rust port.
pub fn small0_registration() -> Result<ModelRegistration, BeatThisError> {
    let beat_model = hash(BEAT_MODEL_SHA256)?;
    let mel_model = hash(MEL_MODEL_SHA256)?;
    let license = LicenseProvenance {
        code: LicenseReference::Spdx("MIT".into()),
        checkpoint: LicenseReference::Spdx("MIT".into()),
        redistribution: Redistribution::RequiresReview,
        source_url: Some(format!("{SOURCE_URL}/tree/{SOURCE_REVISION}")),
        review_notes: "MIT port and ONNX artifacts; authors explicitly note copyrighted and limited-CC training files. User-installed, never bundled automatically.".into(),
    };
    let registration = ModelRegistration {
        manifest: ModelManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            model_id: MODEL_ID.into(),
            architecture: Architecture {
                family: "beat-this".into(),
                version: format!("small-rten@{SOURCE_REVISION}"),
            },
            revision: ExactRevision::Commit(hash(SOURCE_TREE_SHA256)?),
            artifacts: ModelArtifacts {
                weights_sha256: beat_model,
                config_sha256: mel_model,
                adapter_sha256: Some(hash(SOURCE_TREE_SHA256)?),
                conversion_recipe_sha256: Some(hash(CONVERSION_RECIPE_SHA256)?),
                numerical_validation_sha256: Some(hash(NUMERICAL_VALIDATION_SHA256)?),
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
                    runtime: "beat-this-rs@089b509/rten==0.24.0".into(),
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
                    units: "raw frame logits and sample-referenced events".into(),
                },
            },
            golden_validations: Vec::new(),
        },
        artifacts: ArtifactManifest {
            install_directory: INSTALL_DIRECTORY.into(),
            artifacts: vec![
                ArtifactLock {
                    role: ArtifactRole::Weights,
                    relative_path: BEAT_MODEL_FILE.into(),
                    sha256: beat_model,
                    byte_len: Some(BEAT_MODEL_BYTES),
                    required: true,
                },
                ArtifactLock {
                    role: ArtifactRole::Configuration,
                    relative_path: MEL_MODEL_FILE.into(),
                    sha256: mel_model,
                    byte_len: Some(MEL_MODEL_BYTES),
                    required: true,
                },
                ArtifactLock {
                    role: ArtifactRole::Adapter,
                    relative_path: "beat-this-rs.tar".into(),
                    sha256: hash(SOURCE_TREE_SHA256)?,
                    byte_len: Some(17_664_000),
                    required: true,
                },
                ArtifactLock {
                    role: ArtifactRole::ConversionRecipe,
                    relative_path: "ckpt2onnx.py".into(),
                    sha256: hash(CONVERSION_RECIPE_SHA256)?,
                    byte_len: None,
                    required: true,
                },
                ArtifactLock {
                    role: ArtifactRole::NumericalValidation,
                    relative_path: "golden_small.json".into(),
                    sha256: hash(NUMERICAL_VALIDATION_SHA256)?,
                    byte_len: Some(8_436),
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
            runtime: "beat-this-rs-089b509-rten-0.24.0-f32-cpu".into(),
            adapter_sha256: Some(SOURCE_TREE_SHA256.into()),
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
    fn registration_locks_the_two_rten_graphs_and_provenance_bundle() {
        let registration = small0_registration().unwrap();
        registration.validate().unwrap();
        assert_eq!(registration.artifacts.artifacts.len(), 5);
        assert_eq!(
            registration.artifacts.artifacts[0].relative_path,
            PathBuf::from(BEAT_MODEL_FILE)
        );
        assert_eq!(
            registration.artifacts.artifacts[1].relative_path,
            PathBuf::from(MEL_MODEL_FILE)
        );
        assert_eq!(
            install_manifest().destination_directory,
            PathBuf::from(INSTALL_DIRECTORY)
        );
        assert_eq!(install_manifest().artifacts, INSTALL_ARTIFACTS);
        assert_eq!(
            registration.manifest.execution.backend,
            Backend::Cpu {
                runtime: "beat-this-rs@089b509/rten==0.24.0".into(),
                precision: NumericPrecision::Float32,
            }
        );
    }

    #[test]
    fn provider_state_retains_exact_missing_artifacts_without_launching() {
        let root = std::env::temp_dir().join(format!(
            "audec-beat-this-provider-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let registry = ModelRegistry::new(&root);
        let state = provider_state(&registry, "audec-beat-this-worker").unwrap();
        let BeatThisProviderState::Unavailable(InstallStatus::Missing { paths }) = state else {
            panic!("missing installation must remain a typed provider state");
        };
        assert_eq!(paths.len(), INSTALL_ARTIFACTS.len());
        assert!(paths.contains(&PathBuf::from(BEAT_MODEL_FILE)));
        assert!(paths.contains(&PathBuf::from(MEL_MODEL_FILE)));
        std::fs::remove_dir_all(root).unwrap();
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
