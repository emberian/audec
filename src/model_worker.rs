//! UI-independent contracts for optional out-of-process model analysis.
//!
//! This module intentionally does not download or execute models.  It defines
//! the manifest, wire-message, cache-identity, cancellation, and atomic output
//! boundaries that an eventual worker implementation must honor.

use std::fmt;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u32 = 1;
const CACHE_DOMAIN: &[u8] = b"audec.model-worker.cache.v1\0";
const MANIFEST_DOMAIN: &[u8] = b"audec.model-worker.manifest.v1\0";
const PROGRESS_SCALE: u32 = 1_000_000;

/// A SHA-256 content digest, stored as bytes so non-canonical spellings cannot
/// produce distinct cache identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        encode_hex(&self.0)
    }
}

/// SHA-256 of an exact byte representation, without a domain prefix.
///
/// Model material and stored artifacts use ordinary SHA-256 so external
/// workers can verify the same bytes without reimplementing an audec-specific
/// content-addressing envelope.
pub fn sha256_bytes(bytes: &[u8]) -> ContentHash {
    ContentHash(sha256(bytes))
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

impl FromStr for ContentHash {
    type Err = HashParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(HashParseError::WrongLength(value.len()));
        }

        let mut bytes = [0; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = decode_hex_nibble(pair[0]).ok_or(HashParseError::InvalidHex(index * 2))?;
            let low =
                decode_hex_nibble(pair[1]).ok_or(HashParseError::InvalidHex(index * 2 + 1))?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashParseError {
    WrongLength(usize),
    InvalidHex(usize),
}

impl fmt::Display for HashParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength(length) => write!(
                formatter,
                "a SHA-256 digest must contain 64 lowercase hex characters, got {length}"
            ),
            Self::InvalidHex(offset) => {
                write!(formatter, "invalid lowercase hex at byte offset {offset}")
            }
        }
    }
}

impl std::error::Error for HashParseError {}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Architecture {
    /// Published architecture family, for example `htdemucs`.
    pub family: String,
    /// Exact architecture/configuration version, never `latest` or `default`.
    pub version: String,
}

/// An immutable source revision. Both variants include content identity; a
/// release tag alone is not considered pinned because tags can be moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExactRevision {
    Commit(ContentHash),
    Release {
        version: String,
        source_hash: ContentHash,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelArtifacts {
    pub weights_sha256: ContentHash,
    pub config_sha256: ContentHash,
    pub adapter_sha256: Option<ContentHash>,
    pub conversion_recipe_sha256: Option<ContentHash>,
    pub numerical_validation_sha256: Option<ContentHash>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LicenseReference {
    /// A precise SPDX expression, such as `MIT` or `Apache-2.0 AND BSD-3-Clause`.
    Spdx(String),
    /// A non-SPDX license whose exact text is content-addressed.
    Document {
        title: String,
        text_sha256: ContentHash,
    },
    /// Appropriate for private/local models, but never redistributable ones.
    Undeclared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redistribution {
    Forbidden,
    RequiresReview,
    Permitted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LicenseProvenance {
    /// Architecture implementation license. This does not license the weights.
    pub code: LicenseReference,
    /// Checkpoint-specific license governing weights and adapters.
    pub checkpoint: LicenseReference,
    pub redistribution: Redistribution,
    pub source_url: Option<String>,
    pub review_notes: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrainingSource {
    pub name: String,
    pub revision: String,
    pub license: LicenseReference,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrainingProvenance {
    /// A human-readable disclosure. `unknown` is explicit and allowed for a
    /// local model, but is rejected for a redistributable model.
    pub summary: String,
    pub sources: Vec<TrainingSource>,
    /// Hash of the complete model card or training disclosure.
    pub documentation_sha256: ContentHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleEncoding {
    Float32Le,
    SignedPcm16Le,
    SignedPcm24Le,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelContract {
    Mono,
    Stereo,
    Exact(u16),
}

impl ChannelContract {
    fn count(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Exact(count) => count,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioContract {
    pub sample_rate_hz: u32,
    pub channels: ChannelContract,
    pub encoding: SampleEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Normalization {
    None,
    Peak { target_millidbfs: i32 },
    IntegratedLoudness { target_millilufs: i32 },
    StandardScore { mean_bits: u32, stddev_bits: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericPrecision {
    Float32,
    Float16,
    BFloat16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Cpu {
        runtime: String,
        precision: NumericPrecision,
    },
    Cuda {
        runtime: String,
        minimum_compute_capability: (u16, u16),
        precision: NumericPrecision,
    },
    CoreMl {
        runtime: String,
        precision: NumericPrecision,
    },
    Mps {
        runtime: String,
        precision: NumericPrecision,
    },
    Mlx {
        runtime: String,
        precision: NumericPrecision,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionContract {
    pub chunk_frames: u64,
    pub overlap_frames: u64,
    pub normalization: Normalization,
    pub backend: Backend,
    pub estimated_peak_memory_bytes: u64,
    pub required_accelerators: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenValidation {
    pub backend: Backend,
    pub fixture_sha256: ContentHash,
    pub output_sha256: ContentHash,
    pub maximum_error_parts_per_million: u32,
}

/// Mathematical meaning of the outputs. This prevents UI and cache consumers
/// from presenting independent masks or classifier scores as additive stems.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputAdditivity {
    /// Output waveforms sum to the normalized input within the stated bound.
    LinearSum {
        maximum_error_parts_per_million: u32,
    },
    /// Named outputs plus a separately named residual sum to the input.
    LinearSumWithResidual {
        residual_name: String,
        maximum_error_parts_per_million: u32,
    },
    /// Outputs can overlap and must not be summed as disjoint stems.
    OverlappingEstimates { explanation: String },
    /// Output audio is synthesized rather than decomposed from the input.
    Generative { explanation: String },
    /// Outputs are non-audio measurements such as probabilities or embeddings.
    NonAudio { units: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputContract {
    pub names: Vec<String>,
    pub sample_rate_hz: u32,
    pub channels: ChannelContract,
    pub additivity: OutputAdditivity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelManifest {
    pub schema_version: u32,
    pub model_id: String,
    pub architecture: Architecture,
    pub revision: ExactRevision,
    pub artifacts: ModelArtifacts,
    pub license: LicenseProvenance,
    pub training: TrainingProvenance,
    pub input: AudioContract,
    pub execution: ExecutionContract,
    pub output: OutputContract,
    pub golden_validations: Vec<GoldenValidation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

impl ValidationError {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

impl ModelManifest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ValidationError::new(
                "schema_version",
                format!("expected {MANIFEST_SCHEMA_VERSION}"),
            ));
        }
        validate_exact_label("model_id", &self.model_id)?;
        validate_exact_label("architecture.family", &self.architecture.family)?;
        validate_exact_label("architecture.version", &self.architecture.version)?;
        if let ExactRevision::Release { version, .. } = &self.revision {
            validate_exact_label("revision.version", version)?;
        }
        if self.input.sample_rate_hz == 0 {
            return Err(ValidationError::new(
                "input.sample_rate_hz",
                "must be non-zero",
            ));
        }
        if self.input.channels.count() == 0 {
            return Err(ValidationError::new(
                "input.channels",
                "must contain at least one channel",
            ));
        }
        if self.execution.chunk_frames == 0 {
            return Err(ValidationError::new(
                "execution.chunk_frames",
                "must be non-zero",
            ));
        }
        if self.execution.overlap_frames >= self.execution.chunk_frames {
            return Err(ValidationError::new(
                "execution.overlap_frames",
                "must be smaller than the chunk",
            ));
        }
        if let Normalization::StandardScore {
            mean_bits,
            stddev_bits,
        } = self.execution.normalization
        {
            let mean = f32::from_bits(mean_bits);
            let stddev = f32::from_bits(stddev_bits);
            if !mean.is_finite() || !stddev.is_finite() || stddev <= 0.0 {
                return Err(ValidationError::new(
                    "execution.normalization",
                    "standard-score mean must be finite and standard deviation must be finite and positive",
                ));
            }
        }
        validate_backend(&self.execution.backend)?;
        if self.execution.estimated_peak_memory_bytes == 0 {
            return Err(ValidationError::new(
                "execution.estimated_peak_memory_bytes",
                "must be non-zero",
            ));
        }
        for accelerator in &self.execution.required_accelerators {
            validate_nonempty("execution.required_accelerators", accelerator)?;
        }
        let mut accelerators: Vec<&str> = self
            .execution
            .required_accelerators
            .iter()
            .map(String::as_str)
            .collect();
        accelerators.sort_unstable();
        if accelerators.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ValidationError::new(
                "execution.required_accelerators",
                "accelerator requirements must be unique",
            ));
        }
        for validation in &self.golden_validations {
            validate_backend(&validation.backend)?;
        }
        if self.output.sample_rate_hz == 0 || self.output.channels.count() == 0 {
            return Err(ValidationError::new(
                "output",
                "sample rate and channel count must be non-zero",
            ));
        }
        if self.output.names.is_empty() {
            return Err(ValidationError::new(
                "output.names",
                "at least one named output is required",
            ));
        }
        for name in &self.output.names {
            validate_nonempty("output.names", name)?;
        }
        let mut output_names: Vec<&str> = self.output.names.iter().map(String::as_str).collect();
        output_names.sort_unstable();
        if output_names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ValidationError::new(
                "output.names",
                "output names must be unique",
            ));
        }
        validate_additivity(&self.output.additivity)?;
        if let OutputAdditivity::LinearSumWithResidual { residual_name, .. } =
            &self.output.additivity
        {
            if !self.output.names.contains(residual_name) {
                return Err(ValidationError::new(
                    "output.additivity.residual_name",
                    "must name one of the declared outputs",
                ));
            }
        }
        validate_nonempty("training.summary", &self.training.summary)?;

        for source in &self.training.sources {
            validate_nonempty("training.sources.name", &source.name)?;
            validate_exact_label("training.sources.revision", &source.revision)?;
        }

        validate_nonempty("license.review_notes", &self.license.review_notes)?;
        if self
            .license
            .source_url
            .as_deref()
            .is_some_and(|url| url.trim().is_empty())
        {
            return Err(ValidationError::new(
                "license.source_url",
                "must be absent rather than empty",
            ));
        }
        if self.license.redistribution == Redistribution::Permitted {
            if matches!(self.license.code, LicenseReference::Undeclared) {
                return Err(ValidationError::new(
                    "license.code",
                    "redistributable weights require a declared architecture-code license",
                ));
            }
            if matches!(self.license.checkpoint, LicenseReference::Undeclared) {
                return Err(ValidationError::new(
                    "license.checkpoint",
                    "redistributable weights require their own exact SPDX expression or hashed license document",
                ));
            }
            if is_ambiguous(&self.training.summary) {
                return Err(ValidationError::new(
                    "training.summary",
                    "redistributable weights require declared training provenance",
                ));
            }
            if self.training.sources.is_empty() {
                return Err(ValidationError::new(
                    "training.sources",
                    "redistributable weights require declared training sources",
                ));
            }
            for source in &self.training.sources {
                if matches!(source.license, LicenseReference::Undeclared) {
                    return Err(ValidationError::new(
                        "training.sources.license",
                        "redistributable weights cannot cite an unlicensed training source",
                    ));
                }
            }
        }
        validate_license("license.code", &self.license.code)?;
        validate_license("license.checkpoint", &self.license.checkpoint)?;
        for source in &self.training.sources {
            validate_license("training.sources.license", &source.license)?;
        }
        Ok(())
    }

    /// Canonical identity used by `load_model`; independent of any source
    /// material or job recipe.
    pub fn canonical_hash(&self) -> Result<ContentHash, ValidationError> {
        self.validate()?;
        let mut builder = CacheKeyBuilder::with_domain(MANIFEST_DOMAIN);
        self.add_cache_material(&mut builder);
        Ok(builder.finish().digest())
    }

    fn add_cache_material(&self, builder: &mut CacheKeyBuilder) {
        builder.add_u64("manifest.schema", u64::from(self.schema_version));
        builder.add_str("model.id", &self.model_id);
        builder.add_str("architecture.family", &self.architecture.family);
        builder.add_str("architecture.version", &self.architecture.version);
        match &self.revision {
            ExactRevision::Commit(hash) => {
                builder.add_str("revision.kind", "commit");
                builder.add_bytes("revision.hash", hash.as_bytes());
            }
            ExactRevision::Release {
                version,
                source_hash,
            } => {
                builder.add_str("revision.kind", "release");
                builder.add_str("revision.version", version);
                builder.add_bytes("revision.hash", source_hash.as_bytes());
            }
        }
        builder.add_bytes(
            "artifacts.weights",
            self.artifacts.weights_sha256.as_bytes(),
        );
        builder.add_bytes("artifacts.config", self.artifacts.config_sha256.as_bytes());
        builder.add_optional_hash("artifacts.adapter", self.artifacts.adapter_sha256);
        builder.add_optional_hash(
            "artifacts.conversion_recipe",
            self.artifacts.conversion_recipe_sha256,
        );
        builder.add_optional_hash(
            "artifacts.numerical_validation",
            self.artifacts.numerical_validation_sha256,
        );
        add_license(builder, "license.code", &self.license.code);
        add_license(builder, "license.checkpoint", &self.license.checkpoint);
        builder.add_str(
            "license.redistribution",
            match self.license.redistribution {
                Redistribution::Forbidden => "forbidden",
                Redistribution::RequiresReview => "requires-review",
                Redistribution::Permitted => "permitted",
            },
        );
        builder.add_optional_str("license.source_url", self.license.source_url.as_deref());
        builder.add_str("license.review_notes", &self.license.review_notes);
        builder.add_str("training.summary", &self.training.summary);
        builder.add_bytes(
            "training.documentation",
            self.training.documentation_sha256.as_bytes(),
        );
        builder.add_u64("training.sources.count", self.training.sources.len() as u64);
        for (index, source) in self.training.sources.iter().enumerate() {
            let prefix = format!("training.sources.{index}");
            builder.add_str(&format!("{prefix}.name"), &source.name);
            builder.add_str(&format!("{prefix}.revision"), &source.revision);
            add_license(builder, &format!("{prefix}.license"), &source.license);
        }
        builder.add_u64("input.sample_rate", u64::from(self.input.sample_rate_hz));
        builder.add_u64("input.channels", u64::from(self.input.channels.count()));
        builder.add_str("input.encoding", sample_encoding_name(self.input.encoding));
        builder.add_u64("execution.chunk", self.execution.chunk_frames);
        builder.add_u64("execution.overlap", self.execution.overlap_frames);
        add_normalization(builder, self.execution.normalization);
        add_backend(builder, &self.execution.backend);
        builder.add_u64(
            "execution.estimated_peak_memory_bytes",
            self.execution.estimated_peak_memory_bytes,
        );
        let mut accelerators = self.execution.required_accelerators.clone();
        accelerators.sort_unstable();
        builder.add_u64(
            "execution.required_accelerators.count",
            accelerators.len() as u64,
        );
        for (index, accelerator) in accelerators.iter().enumerate() {
            builder.add_str(
                &format!("execution.required_accelerators.{index}"),
                accelerator,
            );
        }
        builder.add_u64("output.sample_rate", u64::from(self.output.sample_rate_hz));
        builder.add_u64("output.channels", u64::from(self.output.channels.count()));
        builder.add_u64("output.names.count", self.output.names.len() as u64);
        for (index, name) in self.output.names.iter().enumerate() {
            builder.add_str(&format!("output.names.{index}"), name);
        }
        add_additivity(builder, &self.output.additivity);
        builder.add_u64(
            "golden_validations.count",
            self.golden_validations.len() as u64,
        );
        for (index, validation) in self.golden_validations.iter().enumerate() {
            let prefix = format!("golden_validations.{index}");
            add_backend_named(builder, &format!("{prefix}.backend"), &validation.backend);
            builder.add_bytes(
                &format!("{prefix}.fixture_sha256"),
                validation.fixture_sha256.as_bytes(),
            );
            builder.add_bytes(
                &format!("{prefix}.output_sha256"),
                validation.output_sha256.as_bytes(),
            );
            builder.add_u64(
                &format!("{prefix}.maximum_error_ppm"),
                u64::from(validation.maximum_error_parts_per_million),
            );
        }
    }
}

fn validate_nonempty(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::new(field, "must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_exact_label(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_nonempty(field, value)?;
    if is_ambiguous(value) {
        Err(ValidationError::new(
            field,
            "must be exact; floating labels such as latest, main, default, or unknown are forbidden",
        ))
    } else {
        Ok(())
    }
}

fn is_ambiguous(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "latest" | "main" | "master" | "head" | "default" | "unknown" | "unspecified" | "n/a"
    )
}

fn validate_license(
    field: &'static str,
    license: &LicenseReference,
) -> Result<(), ValidationError> {
    match license {
        LicenseReference::Spdx(expression) => validate_spdx(field, expression),
        LicenseReference::Document { title, .. } => validate_exact_label(field, title),
        LicenseReference::Undeclared => Ok(()),
    }
}

fn validate_spdx(field: &'static str, expression: &str) -> Result<(), ValidationError> {
    validate_exact_label(field, expression)?;
    let spaced = expression.replace('(', " ( ").replace(')', " ) ");
    let mut expects_identifier = true;
    let mut depth = 0usize;
    for token in spaced.split_whitespace() {
        match token {
            "(" if expects_identifier => depth += 1,
            ")" if !expects_identifier && depth > 0 => depth -= 1,
            "AND" | "OR" | "WITH" if !expects_identifier => expects_identifier = true,
            token if expects_identifier && looks_like_spdx_identifier(token) => {
                expects_identifier = false;
            }
            _ => {
                return Err(ValidationError::new(
                    field,
                    "must be a syntactically precise SPDX expression",
                ));
            }
        }
    }
    if expects_identifier || depth != 0 {
        return Err(ValidationError::new(
            field,
            "must be a complete SPDX expression",
        ));
    }
    Ok(())
}

fn looks_like_spdx_identifier(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'+'))
        && (token.starts_with("LicenseRef-")
            || token.bytes().any(|byte| matches!(byte, b'-' | b'.' | b'+'))
            || token
                .bytes()
                .all(|byte| !byte.is_ascii_alphabetic() || byte.is_ascii_uppercase()))
}

fn validate_backend(backend: &Backend) -> Result<(), ValidationError> {
    let runtime = match backend {
        Backend::Cpu { runtime, .. }
        | Backend::Cuda { runtime, .. }
        | Backend::CoreMl { runtime, .. }
        | Backend::Mps { runtime, .. }
        | Backend::Mlx { runtime, .. } => runtime,
    };
    validate_exact_label("execution.backend.runtime", runtime)
}

fn validate_additivity(additivity: &OutputAdditivity) -> Result<(), ValidationError> {
    match additivity {
        OutputAdditivity::LinearSumWithResidual { residual_name, .. } => {
            validate_nonempty("output.additivity.residual_name", residual_name)
        }
        OutputAdditivity::OverlappingEstimates { explanation }
        | OutputAdditivity::Generative { explanation } => {
            validate_nonempty("output.additivity.explanation", explanation)
        }
        OutputAdditivity::NonAudio { units } => validate_nonempty("output.additivity.units", units),
        OutputAdditivity::LinearSum { .. } => Ok(()),
    }
}

fn sample_encoding_name(encoding: SampleEncoding) -> &'static str {
    match encoding {
        SampleEncoding::Float32Le => "float32-le",
        SampleEncoding::SignedPcm16Le => "pcm-s16-le",
        SampleEncoding::SignedPcm24Le => "pcm-s24-le",
    }
}

fn precision_name(precision: NumericPrecision) -> &'static str {
    match precision {
        NumericPrecision::Float32 => "float32",
        NumericPrecision::Float16 => "float16",
        NumericPrecision::BFloat16 => "bfloat16",
    }
}

fn add_license(builder: &mut CacheKeyBuilder, prefix: &str, license: &LicenseReference) {
    match license {
        LicenseReference::Spdx(expression) => {
            builder.add_str(&format!("{prefix}.kind"), "spdx");
            builder.add_str(&format!("{prefix}.expression"), expression);
        }
        LicenseReference::Document { title, text_sha256 } => {
            builder.add_str(&format!("{prefix}.kind"), "document");
            builder.add_str(&format!("{prefix}.title"), title);
            builder.add_bytes(&format!("{prefix}.text_sha256"), text_sha256.as_bytes());
        }
        LicenseReference::Undeclared => builder.add_str(&format!("{prefix}.kind"), "undeclared"),
    }
}

fn add_normalization(builder: &mut CacheKeyBuilder, normalization: Normalization) {
    match normalization {
        Normalization::None => builder.add_str("normalization.kind", "none"),
        Normalization::Peak { target_millidbfs } => {
            builder.add_str("normalization.kind", "peak");
            builder.add_i64("normalization.target", i64::from(target_millidbfs));
        }
        Normalization::IntegratedLoudness { target_millilufs } => {
            builder.add_str("normalization.kind", "integrated-loudness");
            builder.add_i64("normalization.target", i64::from(target_millilufs));
        }
        Normalization::StandardScore {
            mean_bits,
            stddev_bits,
        } => {
            builder.add_str("normalization.kind", "standard-score");
            builder.add_u64("normalization.mean_bits", u64::from(mean_bits));
            builder.add_u64("normalization.stddev_bits", u64::from(stddev_bits));
        }
    }
}

fn add_backend(builder: &mut CacheKeyBuilder, backend: &Backend) {
    add_backend_named(builder, "backend", backend);
}

fn add_backend_named(builder: &mut CacheKeyBuilder, prefix: &str, backend: &Backend) {
    match backend {
        Backend::Cpu { runtime, precision } => {
            builder.add_str(&format!("{prefix}.kind"), "cpu");
            builder.add_str(&format!("{prefix}.runtime"), runtime);
            builder.add_str(&format!("{prefix}.precision"), precision_name(*precision));
        }
        Backend::Cuda {
            runtime,
            minimum_compute_capability,
            precision,
        } => {
            builder.add_str(&format!("{prefix}.kind"), "cuda");
            builder.add_str(&format!("{prefix}.runtime"), runtime);
            builder.add_u64(
                &format!("{prefix}.compute.major"),
                u64::from(minimum_compute_capability.0),
            );
            builder.add_u64(
                &format!("{prefix}.compute.minor"),
                u64::from(minimum_compute_capability.1),
            );
            builder.add_str(&format!("{prefix}.precision"), precision_name(*precision));
        }
        Backend::CoreMl { runtime, precision } => {
            builder.add_str(&format!("{prefix}.kind"), "coreml");
            builder.add_str(&format!("{prefix}.runtime"), runtime);
            builder.add_str(&format!("{prefix}.precision"), precision_name(*precision));
        }
        Backend::Mps { runtime, precision } => {
            builder.add_str(&format!("{prefix}.kind"), "mps");
            builder.add_str(&format!("{prefix}.runtime"), runtime);
            builder.add_str(&format!("{prefix}.precision"), precision_name(*precision));
        }
        Backend::Mlx { runtime, precision } => {
            builder.add_str(&format!("{prefix}.kind"), "mlx");
            builder.add_str(&format!("{prefix}.runtime"), runtime);
            builder.add_str(&format!("{prefix}.precision"), precision_name(*precision));
        }
    }
}

fn add_additivity(builder: &mut CacheKeyBuilder, additivity: &OutputAdditivity) {
    match additivity {
        OutputAdditivity::LinearSum {
            maximum_error_parts_per_million,
        } => {
            builder.add_str("output.additivity.kind", "linear-sum");
            builder.add_u64(
                "output.additivity.error_ppm",
                u64::from(*maximum_error_parts_per_million),
            );
        }
        OutputAdditivity::LinearSumWithResidual {
            residual_name,
            maximum_error_parts_per_million,
        } => {
            builder.add_str("output.additivity.kind", "linear-sum-with-residual");
            builder.add_str("output.additivity.residual", residual_name);
            builder.add_u64(
                "output.additivity.error_ppm",
                u64::from(*maximum_error_parts_per_million),
            );
        }
        OutputAdditivity::OverlappingEstimates { explanation } => {
            builder.add_str("output.additivity.kind", "overlapping-estimates");
            builder.add_str("output.additivity.explanation", explanation);
        }
        OutputAdditivity::Generative { explanation } => {
            builder.add_str("output.additivity.kind", "generative");
            builder.add_str("output.additivity.explanation", explanation);
        }
        OutputAdditivity::NonAudio { units } => {
            builder.add_str("output.additivity.kind", "non-audio");
            builder.add_str("output.additivity.units", units);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CacheKey(ContentHash);

impl CacheKey {
    pub const fn digest(self) -> ContentHash {
        self.0
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Length-prefixes both names and values, preserving field order and avoiding
/// delimiter ambiguity before applying SHA-256.
#[derive(Clone, Debug)]
pub struct CacheKeyBuilder {
    canonical: Vec<u8>,
}

impl CacheKeyBuilder {
    pub fn new() -> Self {
        let mut builder = Self::with_domain(CACHE_DOMAIN);
        builder.add_u64("protocol.version", u64::from(PROTOCOL_VERSION));
        builder
    }

    fn with_domain(domain: &[u8]) -> Self {
        Self {
            canonical: domain.to_vec(),
        }
    }

    pub fn add_bytes(&mut self, name: &str, value: &[u8]) {
        append_sized(&mut self.canonical, name.as_bytes());
        append_sized(&mut self.canonical, value);
    }

    pub fn add_str(&mut self, name: &str, value: &str) {
        self.add_bytes(name, value.as_bytes());
    }

    pub fn add_u64(&mut self, name: &str, value: u64) {
        self.add_bytes(name, &value.to_be_bytes());
    }

    pub fn add_i64(&mut self, name: &str, value: i64) {
        self.add_bytes(name, &value.to_be_bytes());
    }

    pub fn add_bool(&mut self, name: &str, value: bool) {
        self.add_bytes(name, &[u8::from(value)]);
    }

    pub fn add_optional_str(&mut self, name: &str, value: Option<&str>) {
        match value {
            Some(value) => {
                self.add_bool(&format!("{name}.present"), true);
                self.add_str(name, value);
            }
            None => self.add_bool(&format!("{name}.present"), false),
        }
    }

    pub fn add_optional_hash(&mut self, name: &str, value: Option<ContentHash>) {
        match value {
            Some(value) => {
                self.add_bool(&format!("{name}.present"), true);
                self.add_bytes(name, value.as_bytes());
            }
            None => self.add_bool(&format!("{name}.present"), false),
        }
    }

    pub fn finish(self) -> CacheKey {
        CacheKey(ContentHash(sha256(&self.canonical)))
    }
}

impl Default for CacheKeyBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn append_sized(destination: &mut Vec<u8>, bytes: &[u8]) {
    destination.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    destination.extend_from_slice(bytes);
}

/// A scalar field in a newline-delimited, JSON-object-shaped protocol record.
/// The representation stays dependency-free; a transport adapter can encode
/// these values as JSON without losing integer precision or message typing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolValue {
    String(String),
    Unsigned(u64),
    Bool(bool),
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolRecord {
    pub protocol_version: u32,
    pub sequence: u64,
    pub kind: &'static str,
    pub fields: Vec<(&'static str, ProtocolValue)>,
}

impl ProtocolRecord {
    /// Produces one deterministic JSONL-compatible line without relying on
    /// serde. Field names are static constants owned by this protocol.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "protocol_version",
                format!("expected {PROTOCOL_VERSION}"),
            ));
        }
        let mut names: Vec<&str> = self.fields.iter().map(|(name, _)| *name).collect();
        if names.iter().any(|name| {
            matches!(*name, "protocol_version" | "sequence" | "kind") || name.is_empty()
        }) {
            return Err(ValidationError::new(
                "protocol.fields",
                "field names must be nonempty and cannot replace envelope fields",
            ));
        }
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ValidationError::new(
                "protocol.fields",
                "field names must be unique",
            ));
        }
        Ok(())
    }

    pub fn to_json_line(&self) -> Result<String, ValidationError> {
        self.validate()?;
        let mut line = format!(
            "{{\"protocol_version\":{},\"sequence\":{},\"kind\":\"{}\"",
            self.protocol_version,
            self.sequence,
            escape_json(self.kind)
        );
        for (name, value) in &self.fields {
            line.push_str(",\"");
            line.push_str(&escape_json(name));
            line.push_str("\":");
            match value {
                ProtocolValue::String(value) => {
                    line.push('"');
                    line.push_str(&escape_json(value));
                    line.push('"');
                }
                ProtocolValue::Unsigned(value) => line.push_str(&value.to_string()),
                ProtocolValue::Bool(value) => line.push_str(if *value { "true" } else { "false" }),
                ProtocolValue::Null => line.push_str("null"),
            }
        }
        line.push_str("}\n");
        Ok(line)
    }
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character <= '\u{1f}' => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaterialSpan {
    pub start_frame: u64,
    pub frame_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveParameter {
    pub name: String,
    pub value: ParameterValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParameterValue {
    String(String),
    Signed(i64),
    Unsigned(u64),
    Bool(bool),
    FiniteFloat(FiniteF64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FiniteF64(u64);

impl FiniteF64 {
    pub fn new(value: f64) -> Result<Self, ValidationError> {
        if !value.is_finite() {
            return Err(ValidationError::new(
                "parameter",
                "floating-point parameters must be finite",
            ));
        }
        // Canonicalize negative zero so numerically identical recipes share a key.
        Ok(Self(if value == 0.0 {
            0.0f64.to_bits()
        } else {
            value.to_bits()
        }))
    }

    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeparationRequest {
    pub id: JobId,
    pub model_manifest_sha256: ContentHash,
    pub material_sha256: ContentHash,
    pub span: MaterialSpan,
    pub channel_selection: Vec<u16>,
    pub prompt: Option<String>,
    pub reference_sha256: Vec<ContentHash>,
    pub mask_sha256: Vec<ContentHash>,
    pub parameters: Vec<EffectiveParameter>,
    pub staging: AtomicStaging,
}

impl SeparationRequest {
    fn validate_shape(&self) -> Result<(), ValidationError> {
        validate_nonempty("job_id", &self.id.0)?;
        if self.span.frame_count == 0 {
            return Err(ValidationError::new("span.frame_count", "must be non-zero"));
        }
        if self
            .span
            .start_frame
            .checked_add(self.span.frame_count)
            .is_none()
        {
            return Err(ValidationError::new(
                "span",
                "start plus frame count must fit in u64",
            ));
        }
        self.staging.validate()?;
        let mut names: Vec<&str> = self
            .parameters
            .iter()
            .map(|item| item.name.as_str())
            .collect();
        for name in &names {
            validate_nonempty("parameters.name", name)?;
        }
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ValidationError::new(
                "parameters",
                "effective parameter names must be unique",
            ));
        }
        let mut channels = self.channel_selection.clone();
        channels.sort_unstable();
        if channels.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ValidationError::new(
                "channel_selection",
                "selected channel indices must be unique",
            ));
        }
        Ok(())
    }

    pub fn cache_key(&self, manifest: &ModelManifest) -> Result<CacheKey, ValidationError> {
        manifest.validate()?;
        if self.model_manifest_sha256 != manifest.canonical_hash()? {
            return Err(ValidationError::new(
                "model_manifest_sha256",
                "does not identify the supplied manifest",
            ));
        }
        self.validate_shape()?;

        let mut parameters: Vec<&EffectiveParameter> = self.parameters.iter().collect();
        parameters.sort_by(|left, right| left.name.cmp(&right.name));
        if self
            .channel_selection
            .iter()
            .any(|channel| *channel >= manifest.input.channels.count())
        {
            return Err(ValidationError::new(
                "channel_selection",
                "contains an index outside the model input channel contract",
            ));
        }

        let mut builder = CacheKeyBuilder::new();
        manifest.add_cache_material(&mut builder);
        builder.add_bytes(
            "request.manifest_sha256",
            self.model_manifest_sha256.as_bytes(),
        );
        builder.add_bytes("request.material_sha256", self.material_sha256.as_bytes());
        builder.add_u64("request.start_frame", self.span.start_frame);
        builder.add_u64("request.frame_count", self.span.frame_count);
        builder.add_optional_str("request.prompt", self.prompt.as_deref());
        add_hashes(&mut builder, "request.references", &self.reference_sha256);
        add_hashes(&mut builder, "request.masks", &self.mask_sha256);
        builder.add_u64(
            "request.channels.count",
            self.channel_selection.len() as u64,
        );
        for (index, channel) in self.channel_selection.iter().enumerate() {
            builder.add_u64(&format!("request.channels.{index}"), u64::from(*channel));
        }
        builder.add_u64("request.parameters.count", parameters.len() as u64);
        for (index, parameter) in parameters.into_iter().enumerate() {
            let prefix = format!("request.parameters.{index}");
            builder.add_str(&format!("{prefix}.name"), &parameter.name);
            add_parameter(&mut builder, &prefix, &parameter.value);
        }
        Ok(builder.finish())
    }
}

fn add_hashes(builder: &mut CacheKeyBuilder, prefix: &str, hashes: &[ContentHash]) {
    builder.add_u64(&format!("{prefix}.count"), hashes.len() as u64);
    for (index, hash) in hashes.iter().enumerate() {
        builder.add_bytes(&format!("{prefix}.{index}"), hash.as_bytes());
    }
}

fn add_parameter(builder: &mut CacheKeyBuilder, prefix: &str, value: &ParameterValue) {
    match value {
        ParameterValue::String(value) => {
            builder.add_str(&format!("{prefix}.kind"), "string");
            builder.add_str(&format!("{prefix}.value"), value);
        }
        ParameterValue::Signed(value) => {
            builder.add_str(&format!("{prefix}.kind"), "signed");
            builder.add_i64(&format!("{prefix}.value"), *value);
        }
        ParameterValue::Unsigned(value) => {
            builder.add_str(&format!("{prefix}.kind"), "unsigned");
            builder.add_u64(&format!("{prefix}.value"), *value);
        }
        ParameterValue::Bool(value) => {
            builder.add_str(&format!("{prefix}.kind"), "bool");
            builder.add_bool(&format!("{prefix}.value"), *value);
        }
        ParameterValue::FiniteFloat(value) => {
            builder.add_str(&format!("{prefix}.kind"), "finite-float64");
            builder.add_u64(&format!("{prefix}.bits"), value.0);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerCapabilities {
    pub worker_name: String,
    pub backends: Vec<String>,
    pub maximum_parallel_jobs: u16,
    pub shared_memory: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub name: String,
    pub value: ParameterValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerRequest {
    Hello,
    LoadModel { manifest_sha256: ContentHash },
    Separate(Box<SeparationRequest>),
    Cancel { job_id: JobId },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerResponse {
    Capabilities(WorkerCapabilities),
    ModelLoaded {
        manifest_sha256: ContentHash,
    },
    Progress {
        job_id: JobId,
        completed_chunks: u64,
        total_chunks: u64,
    },
    Complete {
        job_id: JobId,
        staged_artifacts: StagedArtifactSet,
        measurements: Vec<Measurement>,
    },
    Error {
        job_id: JobId,
        kind: String,
        detail: String,
    },
    Cancelled {
        job_id: JobId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerMessage {
    Request(WorkerRequest),
    Response(WorkerResponse),
}

/// One typed message per future JSONL record. Serialization/parsing remains a
/// transport concern; this module fixes direction, version, sequencing, and
/// payload shape without introducing serde into the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolEnvelope {
    pub protocol_version: u32,
    pub sequence: u64,
    pub message: WorkerMessage,
}

impl ProtocolEnvelope {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "protocol_version",
                format!("expected {PROTOCOL_VERSION}"),
            ));
        }
        match &self.message {
            WorkerMessage::Request(WorkerRequest::Hello | WorkerRequest::Shutdown)
            | WorkerMessage::Request(WorkerRequest::LoadModel { .. })
            | WorkerMessage::Response(WorkerResponse::ModelLoaded { .. }) => Ok(()),
            WorkerMessage::Request(WorkerRequest::Separate(request)) => request.validate_shape(),
            WorkerMessage::Request(WorkerRequest::Cancel { job_id })
            | WorkerMessage::Response(WorkerResponse::Cancelled { job_id }) => {
                validate_nonempty("job_id", &job_id.0)
            }
            WorkerMessage::Response(WorkerResponse::Capabilities(capabilities)) => {
                validate_nonempty("capabilities.worker_name", &capabilities.worker_name)?;
                if capabilities.maximum_parallel_jobs == 0 {
                    return Err(ValidationError::new(
                        "capabilities.maximum_parallel_jobs",
                        "must be non-zero",
                    ));
                }
                for backend in &capabilities.backends {
                    validate_nonempty("capabilities.backends", backend)?;
                }
                Ok(())
            }
            WorkerMessage::Response(WorkerResponse::Progress {
                job_id,
                completed_chunks,
                total_chunks,
            }) => {
                validate_nonempty("job_id", &job_id.0)?;
                Progress::from_parts(*completed_chunks, *total_chunks)?;
                Ok(())
            }
            WorkerMessage::Response(WorkerResponse::Complete {
                job_id,
                staged_artifacts,
                measurements,
            }) => {
                validate_nonempty("job_id", &job_id.0)?;
                staged_artifacts.validate_declaration()?;
                for measurement in measurements {
                    validate_nonempty("measurements.name", &measurement.name)?;
                }
                Ok(())
            }
            WorkerMessage::Response(WorkerResponse::Error {
                job_id,
                kind,
                detail,
            }) => {
                validate_nonempty("job_id", &job_id.0)?;
                validate_nonempty("error.kind", kind)?;
                validate_nonempty("error.detail", detail)
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        self.message.kind()
    }
}

impl WorkerMessage {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Request(WorkerRequest::Hello) => "hello",
            Self::Request(WorkerRequest::LoadModel { .. }) => "load_model",
            Self::Request(WorkerRequest::Separate(_)) => "separate",
            Self::Request(WorkerRequest::Cancel { .. }) => "cancel",
            Self::Request(WorkerRequest::Shutdown) => "shutdown",
            Self::Response(WorkerResponse::Capabilities(_)) => "capabilities",
            Self::Response(WorkerResponse::ModelLoaded { .. }) => "model_loaded",
            Self::Response(WorkerResponse::Progress { .. }) => "progress",
            Self::Response(WorkerResponse::Complete { .. }) => "complete",
            Self::Response(WorkerResponse::Error { .. }) => "error",
            Self::Response(WorkerResponse::Cancelled { .. }) => "cancelled",
        }
    }
}

/// Cooperative cancellation shared by the controller and its local protocol
/// pump. An out-of-process transport mirrors a transition to `true` with a
/// [`WorkerRequest::Cancel`] record.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn request(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Staging and destination directories must share a parent so a whole result
/// set is published by one rename. The worker writes only `staging_path`; the
/// controller verifies every declared artifact and performs the commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtomicStaging {
    pub staging_path: PathBuf,
    pub destination_path: PathBuf,
}

impl AtomicStaging {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.staging_path == self.destination_path {
            return Err(ValidationError::new(
                "staging_path",
                "must differ from the destination",
            ));
        }
        let staging_parent = explicit_parent(&self.staging_path);
        let destination_parent = explicit_parent(&self.destination_path);
        if staging_parent != destination_parent {
            return Err(ValidationError::new(
                "staging_path",
                "must share the destination parent for an atomic directory rename",
            ));
        }
        if self.staging_path.file_name().is_none() || self.destination_path.file_name().is_none() {
            return Err(ValidationError::new(
                "staging_path",
                "both paths must name directory entries",
            ));
        }
        for path in [&self.staging_path, &self.destination_path] {
            if path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
            {
                return Err(ValidationError::new(
                    "staging_path",
                    "paths must be lexically normalized before atomic publication",
                ));
            }
        }
        Ok(())
    }

    pub const fn commit_contract() -> [AtomicCommitStep; 5] {
        [
            AtomicCommitStep::WriteStaging,
            AtomicCommitStep::FlushStaging,
            AtomicCommitStep::VerifyDigest,
            AtomicCommitStep::RenameIntoPlace,
            AtomicCommitStep::FlushParentDirectory,
        ]
    }
}

fn explicit_parent(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new(""))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicCommitStep {
    WriteStaging,
    FlushStaging,
    VerifyDigest,
    RenameIntoPlace,
    FlushParentDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedArtifact {
    /// Path relative to the staging directory; absolute paths and traversal
    /// components are forbidden.
    pub relative_path: PathBuf,
    pub sha256: ContentHash,
    pub byte_len: u64,
}

impl StagedArtifact {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.relative_path.as_os_str().is_empty()
            || self.relative_path.components().any(|component| {
                matches!(
                    component,
                    Component::Prefix(_)
                        | Component::RootDir
                        | Component::ParentDir
                        | Component::CurDir
                )
            })
        {
            return Err(ValidationError::new(
                "artifact.relative_path",
                "must be a normalized relative path inside the staging directory",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedArtifactSet {
    pub staging_directory: PathBuf,
    pub artifacts: Vec<StagedArtifact>,
}

impl StagedArtifactSet {
    pub fn validate(&self, plan: &AtomicStaging) -> Result<(), ValidationError> {
        plan.validate()?;
        if self.staging_directory != plan.staging_path {
            return Err(ValidationError::new(
                "staged_artifacts.staging_directory",
                "must match the atomic staging plan",
            ));
        }
        self.validate_declaration()
    }

    fn validate_declaration(&self) -> Result<(), ValidationError> {
        if self.staging_directory.as_os_str().is_empty() {
            return Err(ValidationError::new(
                "staged_artifacts.staging_directory",
                "must not be empty",
            ));
        }
        if self.artifacts.is_empty() {
            return Err(ValidationError::new(
                "staged_artifacts",
                "must declare at least one output",
            ));
        }
        for artifact in &self.artifacts {
            artifact.validate()?;
        }
        let mut paths: Vec<&Path> = self
            .artifacts
            .iter()
            .map(|artifact| artifact.relative_path.as_path())
            .collect();
        paths.sort_unstable();
        if paths.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ValidationError::new(
                "staged_artifacts",
                "relative output paths must be unique",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobPhase {
    Preparing,
    Decoding,
    Analyzing,
    Encoding,
    Verifying,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    completed_chunks: u64,
    total_chunks: u64,
}

impl Progress {
    /// No progress report has established a total yet.
    pub const ZERO: Self = Self {
        completed_chunks: 0,
        total_chunks: 0,
    };

    pub fn from_parts(completed: u64, total: u64) -> Result<Self, ValidationError> {
        if total == 0 || completed > total {
            return Err(ValidationError::new(
                "progress",
                "requires 0 <= completed <= total and a non-zero total",
            ));
        }
        Ok(Self {
            completed_chunks: completed,
            total_chunks: total,
        })
    }

    pub fn parts_per_million(self) -> u32 {
        if self.total_chunks == 0 {
            return 0;
        }
        ((u128::from(self.completed_chunks) * u128::from(PROGRESS_SCALE))
            / u128::from(self.total_chunks)) as u32
    }

    pub const fn completed_chunks(self) -> u64 {
        self.completed_chunks
    }

    pub const fn total_chunks(self) -> u64 {
        self.total_chunks
    }

    pub const fn is_complete(self) -> bool {
        self.total_chunks != 0 && self.completed_chunks == self.total_chunks
    }
}

fn progress_follows(previous: Progress, next: Progress) -> bool {
    (previous.total_chunks == 0 || previous.total_chunks == next.total_chunks)
        && next.completed_chunks >= previous.completed_chunks
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running { phase: JobPhase, progress: Progress },
    Cancelling { phase: JobPhase, progress: Progress },
    CommitReady(StagedArtifactSet),
    Committing(StagedArtifactSet),
    Succeeded(StagedArtifactSet),
    Cancelled,
    Failed(String),
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded(_) | Self::Cancelled | Self::Failed(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionError {
    pub action: &'static str,
    pub state: JobState,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot {} while job is {:?}",
            self.action, self.state
        )
    }
}

impl std::error::Error for TransitionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobLifecycle {
    pub id: JobId,
    staging: AtomicStaging,
    state: JobState,
}

impl JobLifecycle {
    pub fn new(id: JobId, staging: AtomicStaging) -> Result<Self, ValidationError> {
        validate_nonempty("job_id", &id.0)?;
        staging.validate()?;
        Ok(Self {
            id,
            staging,
            state: JobState::Queued,
        })
    }

    pub fn state(&self) -> &JobState {
        &self.state
    }

    pub fn start(&mut self) -> Result<(), TransitionError> {
        if self.state != JobState::Queued {
            return Err(self.error("start"));
        }
        self.state = JobState::Running {
            phase: JobPhase::Preparing,
            progress: Progress::ZERO,
        };
        Ok(())
    }

    pub fn report_progress(
        &mut self,
        phase: JobPhase,
        progress: Progress,
    ) -> Result<(), TransitionError> {
        let cancelling = match &self.state {
            JobState::Running {
                phase: previous_phase,
                progress: previous_progress,
            } => {
                if phase < *previous_phase || !progress_follows(*previous_progress, progress) {
                    return Err(self.error("report regressing progress"));
                }
                false
            }
            JobState::Cancelling {
                phase: previous_phase,
                progress: previous_progress,
            } => {
                if phase < *previous_phase || !progress_follows(*previous_progress, progress) {
                    return Err(self.error("report regressing progress"));
                }
                true
            }
            _ => return Err(self.error("report progress")),
        };
        self.state = if cancelling {
            JobState::Cancelling { phase, progress }
        } else {
            JobState::Running { phase, progress }
        };
        Ok(())
    }

    /// Returns `true` only for the first effective cancellation request.
    pub fn request_cancel(&mut self) -> Result<bool, TransitionError> {
        match &self.state {
            JobState::Queued => {
                self.state = JobState::Cancelled;
                Ok(true)
            }
            JobState::Running { phase, progress } => {
                self.state = JobState::Cancelling {
                    phase: *phase,
                    progress: *progress,
                };
                Ok(true)
            }
            JobState::CommitReady(_) => {
                self.state = JobState::Cancelled;
                Ok(true)
            }
            JobState::Cancelling { .. } | JobState::Cancelled => Ok(false),
            _ => Err(self.error("cancel")),
        }
    }

    pub fn acknowledge_cancelled(&mut self) -> Result<(), TransitionError> {
        if !matches!(self.state, JobState::Cancelling { .. }) {
            return Err(self.error("acknowledge cancellation"));
        }
        self.state = JobState::Cancelled;
        Ok(())
    }

    pub fn mark_commit_ready(
        &mut self,
        artifacts: StagedArtifactSet,
    ) -> Result<(), TransitionError> {
        if artifacts.validate(&self.staging).is_err() {
            return Err(self.error("accept invalid staged output"));
        }
        match &self.state {
            JobState::Running { phase, progress }
                if *phase == JobPhase::Verifying && progress.is_complete() =>
            {
                self.state = JobState::CommitReady(artifacts);
                Ok(())
            }
            _ => Err(self.error("mark output ready to commit")),
        }
    }

    /// Claims the commit boundary immediately before filesystem publication.
    /// Cancellation is rejected from this point to avoid a published result
    /// whose lifecycle says `Cancelled`.
    pub fn begin_commit(&mut self) -> Result<(), TransitionError> {
        let JobState::CommitReady(artifact) = &self.state else {
            return Err(self.error("begin atomic commit"));
        };
        self.state = JobState::Committing(artifact.clone());
        Ok(())
    }

    /// Called only after the controller completes [`AtomicStaging::commit_contract`].
    pub fn commit_succeeded(&mut self) -> Result<(), TransitionError> {
        let JobState::Committing(artifact) = &self.state else {
            return Err(self.error("complete atomic commit"));
        };
        self.state = JobState::Succeeded(artifact.clone());
        Ok(())
    }

    pub fn fail(&mut self, message: impl Into<String>) -> Result<(), TransitionError> {
        if self.state.is_terminal() {
            return Err(self.error("fail"));
        }
        self.state = JobState::Failed(message.into());
        Ok(())
    }

    fn error(&self, action: &'static str) -> TransitionError {
        TransitionError {
            action,
            state: self.state.clone(),
        }
    }
}

// Small dependency-free SHA-256 used solely for canonical cache identities.
fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for block in padded.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (index, word) in block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temporary1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    fn manifest() -> ModelManifest {
        ModelManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            model_id: "org.example/separator".into(),
            architecture: Architecture {
                family: "transformer-separator".into(),
                version: "2.1.0".into(),
            },
            revision: ExactRevision::Release {
                version: "2026.08".into(),
                source_hash: hash(1),
            },
            artifacts: ModelArtifacts {
                weights_sha256: hash(2),
                config_sha256: hash(3),
                adapter_sha256: None,
                conversion_recipe_sha256: Some(hash(5)),
                numerical_validation_sha256: Some(hash(6)),
            },
            license: LicenseProvenance {
                code: LicenseReference::Spdx("MIT".into()),
                checkpoint: LicenseReference::Spdx("Apache-2.0".into()),
                redistribution: Redistribution::Permitted,
                source_url: Some("https://example.invalid/model".into()),
                review_notes: "Checkpoint license confirmed by its publisher".into(),
            },
            training: TrainingProvenance {
                summary: "Licensed synthetic multitrack corpus".into(),
                sources: vec![TrainingSource {
                    name: "example-corpus".into(),
                    revision: "2026.04".into(),
                    license: LicenseReference::Spdx("CC0-1.0".into()),
                }],
                documentation_sha256: hash(4),
            },
            input: AudioContract {
                sample_rate_hz: 44_100,
                channels: ChannelContract::Stereo,
                encoding: SampleEncoding::Float32Le,
            },
            execution: ExecutionContract {
                chunk_frames: 441_000,
                overlap_frames: 44_100,
                normalization: Normalization::Peak {
                    target_millidbfs: -1_000,
                },
                backend: Backend::Cpu {
                    runtime: "onnx-runtime-1.20.1".into(),
                    precision: NumericPrecision::Float32,
                },
                estimated_peak_memory_bytes: 1_073_741_824,
                required_accelerators: Vec::new(),
            },
            output: OutputContract {
                names: vec!["vocals".into(), "accompaniment".into()],
                sample_rate_hz: 44_100,
                channels: ChannelContract::Stereo,
                additivity: OutputAdditivity::LinearSum {
                    maximum_error_parts_per_million: 10,
                },
            },
            golden_validations: vec![GoldenValidation {
                backend: Backend::Cpu {
                    runtime: "onnx-runtime-1.20.1".into(),
                    precision: NumericPrecision::Float32,
                },
                fixture_sha256: hash(15),
                output_sha256: hash(16),
                maximum_error_parts_per_million: 25,
            }],
        }
    }

    fn request() -> SeparationRequest {
        SeparationRequest {
            id: JobId("job-cache".into()),
            model_manifest_sha256: manifest().canonical_hash().unwrap(),
            material_sha256: hash(11),
            span: MaterialSpan {
                start_frame: 100,
                frame_count: 4_410,
            },
            channel_selection: vec![1, 0],
            prompt: Some("dry vocal".into()),
            reference_sha256: vec![hash(13), hash(12)],
            mask_sha256: vec![hash(14)],
            parameters: vec![
                EffectiveParameter {
                    name: "temperature".into(),
                    value: ParameterValue::FiniteFloat(FiniteF64::new(0.25).unwrap()),
                },
                EffectiveParameter {
                    name: "seed".into(),
                    value: ParameterValue::Unsigned(7),
                },
            ],
            staging: AtomicStaging {
                staging_path: "cache/.job-cache.tmp".into(),
                destination_path: "cache/job-cache".into(),
            },
        }
    }

    #[test]
    fn hash_parser_requires_canonical_lowercase_sha256() {
        let text = "ab".repeat(32);
        assert_eq!(ContentHash::from_str(&text).unwrap().to_string(), text);
        assert!(ContentHash::from_str(&"AB".repeat(32)).is_err());
        assert!(ContentHash::from_str("abc").is_err());
    }

    #[test]
    fn redistributable_manifest_rejects_ambiguous_provenance() {
        let mut candidate = manifest();
        candidate.architecture.version = "latest".into();
        assert_eq!(
            candidate.validate().unwrap_err().field,
            "architecture.version"
        );

        let mut candidate = manifest();
        candidate.license.checkpoint = LicenseReference::Undeclared;
        assert_eq!(
            candidate.validate().unwrap_err().field,
            "license.checkpoint"
        );

        let mut candidate = manifest();
        candidate.training.summary = "unknown".into();
        assert_eq!(candidate.validate().unwrap_err().field, "training.summary");
    }

    #[test]
    fn manifest_rejects_invalid_audio_chunk_contract() {
        let mut candidate = manifest();
        candidate.execution.overlap_frames = candidate.execution.chunk_frames;
        assert_eq!(
            candidate.validate().unwrap_err().field,
            "execution.overlap_frames"
        );

        let mut candidate = manifest();
        candidate.execution.normalization = Normalization::StandardScore {
            mean_bits: f32::NAN.to_bits(),
            stddev_bits: 1.0f32.to_bits(),
        };
        assert_eq!(
            candidate.validate().unwrap_err().field,
            "execution.normalization"
        );

        let mut candidate = manifest();
        candidate.license.checkpoint = LicenseReference::Spdx("see repository".into());
        assert_eq!(
            candidate.validate().unwrap_err().field,
            "license.checkpoint"
        );
    }

    #[test]
    fn cache_key_is_stable_and_length_prefixing_is_unambiguous() {
        let first = request().cache_key(&manifest()).unwrap();
        let second = request().cache_key(&manifest()).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.to_string(),
            "7cbcc02ad4ecc7bdf23ebb69f952709f4b1f84366694e86f2c8160f8278ff7be"
        );

        let mut left = CacheKeyBuilder::new();
        left.add_str("a", "bc");
        let mut right = CacheKeyBuilder::new();
        right.add_str("ab", "c");
        assert_ne!(left.finish(), right.finish());
    }

    #[test]
    fn separation_key_canonicalizes_parameters_and_preserves_ordered_inputs() {
        let baseline = request();
        let key = baseline.cache_key(&manifest()).unwrap();

        let mut reordered_parameters = baseline.clone();
        reordered_parameters.parameters.reverse();
        assert_eq!(key, reordered_parameters.cache_key(&manifest()).unwrap());

        let mut reordered_channels = baseline.clone();
        reordered_channels.channel_selection.reverse();
        assert_ne!(key, reordered_channels.cache_key(&manifest()).unwrap());

        let mut reordered_references = baseline.clone();
        reordered_references.reference_sha256.reverse();
        assert_ne!(key, reordered_references.cache_key(&manifest()).unwrap());

        let mut changed = baseline.clone();
        changed.prompt = Some("wet vocal".into());
        assert_ne!(key, changed.cache_key(&manifest()).unwrap());

        let mut duplicate = baseline;
        duplicate.parameters.push(duplicate.parameters[0].clone());
        assert_eq!(
            duplicate.cache_key(&manifest()).unwrap_err().field,
            "parameters"
        );
        assert_eq!(FiniteF64::new(-0.0), FiniteF64::new(0.0));
        assert!(FiniteF64::new(f64::NAN).is_err());

        let mut mismatched_manifest = request();
        mismatched_manifest.model_manifest_sha256 = hash(99);
        assert_eq!(
            mismatched_manifest
                .cache_key(&manifest())
                .unwrap_err()
                .field,
            "model_manifest_sha256"
        );
    }

    #[test]
    fn cache_key_changes_for_contract_artifact_and_range() {
        let baseline = manifest();
        let request = request();
        let key = request.cache_key(&baseline).unwrap();

        let mut changed_artifact = baseline.clone();
        changed_artifact.artifacts.weights_sha256 = hash(8);
        let mut changed_request = request.clone();
        changed_request.model_manifest_sha256 = changed_artifact.canonical_hash().unwrap();
        assert_ne!(key, changed_request.cache_key(&changed_artifact).unwrap());

        let mut changed_contract = baseline.clone();
        changed_contract.execution.overlap_frames += 1;
        let mut changed_request = request.clone();
        changed_request.model_manifest_sha256 = changed_contract.canonical_hash().unwrap();
        assert_ne!(key, changed_request.cache_key(&changed_contract).unwrap());
        let mut changed_range = request;
        changed_range.span.start_frame += 1;
        assert_ne!(key, changed_range.cache_key(&baseline).unwrap());
    }

    #[test]
    fn protocol_record_is_one_escaped_json_line() {
        let record = ProtocolRecord {
            protocol_version: PROTOCOL_VERSION,
            sequence: 7,
            kind: "failed",
            fields: vec![("error", ProtocolValue::String("bad\n\"model\"".into()))],
        };
        assert_eq!(
            record.to_json_line().unwrap(),
            "{\"protocol_version\":1,\"sequence\":7,\"kind\":\"failed\",\"error\":\"bad\\n\\\"model\\\"\"}\n"
        );
        let duplicate = ProtocolRecord {
            protocol_version: PROTOCOL_VERSION,
            sequence: 8,
            kind: "invalid",
            fields: vec![
                ("job", ProtocolValue::String("one".into())),
                ("job", ProtocolValue::String("two".into())),
            ],
        };
        assert!(duplicate.to_json_line().is_err());

        let invalid_progress = ProtocolEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence: 9,
            message: WorkerMessage::Response(WorkerResponse::Progress {
                job_id: JobId("job-9".into()),
                completed_chunks: 2,
                total_chunks: 1,
            }),
        };
        assert!(invalid_progress.validate().is_err());
    }

    #[test]
    fn lifecycle_requires_verified_atomic_commit_before_success() {
        let staging = AtomicStaging {
            staging_path: "results/.job-1.tmp".into(),
            destination_path: "results/job-1".into(),
        };
        let mut job = JobLifecycle::new(JobId("job-1".into()), staging).unwrap();
        assert!(job.commit_succeeded().is_err());
        job.start().unwrap();
        job.report_progress(JobPhase::Analyzing, Progress::from_parts(1, 2).unwrap())
            .unwrap();
        assert!(job
            .report_progress(JobPhase::Decoding, Progress::from_parts(1, 4).unwrap())
            .is_err());
        job.report_progress(JobPhase::Verifying, Progress::from_parts(2, 2).unwrap())
            .unwrap();

        let artifact = StagedArtifactSet {
            staging_directory: PathBuf::from("results/.job-1.tmp"),
            artifacts: vec![StagedArtifact {
                relative_path: PathBuf::from("vocals.f32"),
                sha256: hash(7),
                byte_len: 42,
            }],
        };
        let invalid_artifact = StagedArtifactSet {
            staging_directory: PathBuf::from("results/.job-1.tmp"),
            artifacts: vec![StagedArtifact {
                relative_path: PathBuf::from("../escape.f32"),
                sha256: hash(7),
                byte_len: 42,
            }],
        };
        assert!(job.mark_commit_ready(invalid_artifact).is_err());
        job.mark_commit_ready(artifact.clone()).unwrap();
        assert_eq!(job.state(), &JobState::CommitReady(artifact.clone()));
        job.begin_commit().unwrap();
        assert!(job.request_cancel().is_err());
        job.commit_succeeded().unwrap();
        assert_eq!(job.state(), &JobState::Succeeded(artifact));
    }

    #[test]
    fn cancellation_is_idempotent_and_terminal() {
        let staging = AtomicStaging {
            staging_path: "results/.job-2.tmp".into(),
            destination_path: "results/job-2".into(),
        };
        let mut job = JobLifecycle::new(JobId("job-2".into()), staging).unwrap();
        job.start().unwrap();
        assert!(job.request_cancel().unwrap());
        assert!(!job.request_cancel().unwrap());
        job.acknowledge_cancelled().unwrap();
        assert!(job.state().is_terminal());
        assert!(job.fail("too late").is_err());

        let token = CancellationToken::default();
        assert!(token.request());
        assert!(!token.request());
        assert!(token.is_requested());
    }

    #[test]
    fn atomic_staging_requires_a_shared_directory() {
        let valid = AtomicStaging {
            staging_path: "cache/.result.tmp".into(),
            destination_path: "cache/result.bin".into(),
        };
        assert!(valid.validate().is_ok());
        let invalid = AtomicStaging {
            staging_path: "tmp/result.tmp".into(),
            destination_path: "cache/result.bin".into(),
        };
        assert!(invalid.validate().is_err());
        assert_eq!(
            AtomicStaging::commit_contract()[3],
            AtomicCommitStep::RenameIntoPlace
        );
    }

    #[test]
    fn sha256_matches_published_empty_input_vector() {
        assert_eq!(
            encode_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
