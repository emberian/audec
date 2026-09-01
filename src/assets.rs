//! Project media-pool primitives.
//!
//! This module deliberately does not open files or decode audio.  The UI or an
//! importer supplies a decoded description, a content fingerprint, and later
//! reports whether a location can still be resolved.  Keeping that boundary
//! pure makes manifests deterministic, makes relinking testable, and keeps
//! filesystem work out of the project model.
//!
//! `ContentId` currently uses FNV-1a 128.  It is stable and useful for finding
//! accidental duplicate imports, but it is **not cryptographic** and must not
//! be used to establish authenticity or make an irreversible deduplication
//! decision without byte comparison (or a stronger future fingerprint).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// A project-stable media-pool key. Values are allocated monotonically and
/// never reused by [`AssetRegistry`].
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AssetId(pub u64);

/// A project-stable usage key, allocated independently of asset IDs.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AssetUsageId(pub u64);

/// Exact audio frame count. This is intentionally not a floating point
/// duration: a clip can always be expressed as a half-open range of frames.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SampleFrames(pub u64);

/// A stable, non-cryptographic content identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentId(pub u128);

impl ContentId {
    /// FNV-1a 128 over the supplied bytes. Its output is deterministic across
    /// platforms, but collision resistance is intentionally *not* promised.
    pub fn fnv1a_128(bytes: &[u8]) -> Self {
        const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
        const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
        let mut hash = OFFSET;
        for byte in bytes {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        Self(hash)
    }

    pub fn to_hex(self) -> String {
        format!("{:032x}", self.0)
    }
}

/// The algorithm used to form a [`ContentFingerprint`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ContentHashAlgorithm {
    /// Stable duplicate hint only. It is not collision-resistant and is not a
    /// security boundary.
    Fnv1a128NonCryptographic,
}

/// A fingerprint is accompanied by the byte count so callers can reject an
/// obviously incomplete hash before suggesting a relink.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentFingerprint {
    pub algorithm: ContentHashAlgorithm,
    pub id: ContentId,
    pub bytes_hashed: u64,
}

impl ContentFingerprint {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            algorithm: ContentHashAlgorithm::Fnv1a128NonCryptographic,
            id: ContentId::fnv1a_128(bytes),
            bytes_hashed: bytes.len() as u64,
        }
    }
}

/// An absolute path preserved exactly as supplied by the importer. It is not
/// canonicalized, because canonicalization can make a missing asset disappear
/// from a manifest and follows symlinks differently on different machines.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AbsolutePath(String);

impl AbsolutePath {
    pub fn parse(path: impl Into<String>) -> Result<Self, AssetError> {
        let path = path.into();
        if Path::new(&path).is_absolute() {
            Ok(Self(path))
        } else {
            Err(AssetError::PathMustBeAbsolute(path))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

/// A path below a project root. Parent traversal is rejected so persisted
/// projects do not silently acquire a machine-dependent escape hatch.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectRelativePath(String);

impl ProjectRelativePath {
    pub fn parse(path: impl Into<String>) -> Result<Self, AssetError> {
        let path = path.into();
        let candidate = Path::new(&path);
        if path.is_empty() || candidate.is_absolute() {
            return Err(AssetError::InvalidProjectRelativePath(path));
        }
        if candidate.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(AssetError::InvalidProjectRelativePath(path));
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn resolve_from(&self, project_root: &Path) -> PathBuf {
        project_root.join(&self.0)
    }
}

/// The known routes to a single source file. An import may carry both its
/// original absolute path and a portable project-relative copy.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AssetLocation {
    pub absolute: Option<AbsolutePath>,
    pub project_relative: Option<ProjectRelativePath>,
}

impl AssetLocation {
    pub fn new(
        absolute: Option<AbsolutePath>,
        project_relative: Option<ProjectRelativePath>,
    ) -> Result<Self, AssetError> {
        let location = Self {
            absolute,
            project_relative,
        };
        location.validate()?;
        Ok(location)
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        if self.absolute.is_none() && self.project_relative.is_none() {
            return Err(AssetError::LocationHasNoRoute);
        }
        Ok(())
    }

    /// Returns routes in deterministic, portability-first order.
    pub fn candidate_paths(&self, project_root: Option<&Path>) -> Vec<PathBuf> {
        let mut result = Vec::new();
        if let (Some(relative), Some(root)) = (&self.project_relative, project_root) {
            result.push(relative.resolve_from(root));
        }
        if let Some(absolute) = &self.absolute {
            let absolute = absolute.as_path().to_path_buf();
            if !result.contains(&absolute) {
                result.push(absolute);
            }
        }
        result
    }

    fn stable_label(&self) -> String {
        self.project_relative
            .as_ref()
            .map(|path| path.as_str())
            .or_else(|| self.absolute.as_ref().map(|path| path.as_str()))
            .unwrap_or("")
            .to_owned()
    }
}

/// The decoded properties needed to place audio exactly on the project
/// timeline. `frame_count` is per channel, never an interleaved sample count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedAudioMetadata {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frame_count: SampleFrames,
    pub container: Option<String>,
    pub codec: Option<String>,
    pub bit_depth: Option<u16>,
}

impl DecodedAudioMetadata {
    pub fn validate(&self) -> Result<(), AssetError> {
        if self.sample_rate_hz == 0 {
            return Err(AssetError::InvalidAudioMetadata("sample rate is zero"));
        }
        if self.channels == 0 {
            return Err(AssetError::InvalidAudioMetadata("channel count is zero"));
        }
        if self.frame_count.0 == 0 {
            return Err(AssetError::InvalidAudioMetadata("frame count is zero"));
        }
        if self.bit_depth == Some(0) {
            return Err(AssetError::InvalidAudioMetadata("bit depth is zero"));
        }
        Ok(())
    }
}

/// A half-open source interval in an asset, expressed in decoded frames.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AssetFrameRange {
    pub start: SampleFrames,
    pub end: SampleFrames,
}

impl AssetFrameRange {
    pub fn new(start: SampleFrames, end: SampleFrames) -> Result<Self, AssetError> {
        if start >= end {
            return Err(AssetError::InvalidFrameRange { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn len(self) -> SampleFrames {
        SampleFrames(self.end.0 - self.start.0)
    }

    pub fn is_within(self, frame_count: SampleFrames) -> bool {
        self.end <= frame_count
    }
}

/// Why this immutable source first entered the project.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetOrigin {
    ImportedFile {
        importer: String,
    },
    RecordedInput {
        device: String,
    },
    Rendered {
        renderer: String,
        source_revision: u64,
    },
    Generated {
        generator: String,
    },
    Migrated {
        source_format: String,
    },
}

/// Import provenance is deliberately private inside [`MediaAsset`]: an asset
/// can be relinked, tagged, or used elsewhere, but its origin must not be
/// rewritten to make a later path look like its birth place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetProvenance {
    imported_at_unix_ms: u64,
    origin: AssetOrigin,
    original_location: AssetLocation,
    materialization: Option<PcmMaterializationProvenance>,
}

/// Durable account of the encoded stream which supplied an imported PCM
/// asset. These are source facts used for reopening and relinking; they are
/// intentionally distinct from [`MediaAsset::metadata`] and
/// [`MediaAsset::content`], which describe the reusable PCM produced by the
/// materialization recipe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceDecodeProvenance {
    pub backend: String,
    pub backend_version: String,
    pub source_bytes: u64,
    pub stream_count: u32,
    pub selected_track_id: u32,
    pub container: Option<String>,
    pub codec: String,
    pub declared_frames: Option<u64>,
    pub gapless: bool,
    pub verification: DecodeIntegrity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeIntegrity {
    Passed,
    Unavailable,
}

/// Complete, owned sample-rate recipe needed to reproduce imported PCM.
/// Floating-point cutoff is stored by bits so equality and persistence never
/// depend on JSON number formatting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SampleRateMaterializationRecipe {
    pub backend: String,
    pub backend_version: String,
    pub algorithm: String,
    pub input_sample_rate_hz: u32,
    pub output_sample_rate_hz: u32,
    pub channels: u16,
    pub input_frames: u64,
    pub output_frames: u64,
    pub chunk_frames: usize,
    pub sinc_length: usize,
    pub cutoff_bits: Option<u32>,
    pub oversampling_factor: usize,
    pub interpolation: String,
    pub window: String,
    /// `rubato::Resampler::process_all` removes its filter delay. Keeping the
    /// fact explicit prevents a future streaming adapter from silently using
    /// differently aligned output under the same recipe identity.
    pub delay_removed: bool,
    /// Exact endpoint trimming applied after backend processing.
    pub trimmed_output_frames: u64,
}

/// Reproducible source-to-PCM identity for an imported asset. The source
/// fingerprint hashes encoded bytes; the parent asset fingerprint hashes
/// canonical finite interleaved f32 PCM after this recipe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmMaterializationProvenance {
    pub source_metadata: DecodedAudioMetadata,
    pub source_content: ContentFingerprint,
    pub decode: SourceDecodeProvenance,
    pub sample_rate: Option<SampleRateMaterializationRecipe>,
}

impl AssetProvenance {
    pub fn new(
        imported_at_unix_ms: u64,
        origin: AssetOrigin,
        original_location: AssetLocation,
    ) -> Self {
        Self {
            imported_at_unix_ms,
            origin,
            original_location,
            materialization: None,
        }
    }

    pub fn with_materialization(mut self, materialization: PcmMaterializationProvenance) -> Self {
        self.materialization = Some(materialization);
        self
    }

    pub(crate) fn with_optional_materialization(
        mut self,
        materialization: Option<PcmMaterializationProvenance>,
    ) -> Self {
        self.materialization = materialization;
        self
    }

    pub fn imported_at_unix_ms(&self) -> u64 {
        self.imported_at_unix_ms
    }

    pub fn origin(&self) -> &AssetOrigin {
        &self.origin
    }

    pub fn original_location(&self) -> &AssetLocation {
        &self.original_location
    }

    pub fn materialization(&self) -> Option<&PcmMaterializationProvenance> {
        self.materialization.as_ref()
    }

    fn validate_for_asset(
        &self,
        output_metadata: &DecodedAudioMetadata,
        output_content: ContentFingerprint,
    ) -> Result<(), AssetError> {
        let Some(materialization) = &self.materialization else {
            return Ok(());
        };
        materialization.source_metadata.validate()?;
        if materialization.source_content.bytes_hashed == 0 {
            return Err(AssetError::InvalidProvenance(
                "materialization source fingerprint is empty",
            ));
        }
        let decode = &materialization.decode;
        if decode.backend.trim().is_empty()
            || decode.backend_version.trim().is_empty()
            || decode.codec.trim().is_empty()
            || decode.source_bytes == 0
            || decode.stream_count == 0
        {
            return Err(AssetError::InvalidProvenance(
                "materialization decode recipe is incomplete",
            ));
        }
        if decode.source_bytes != materialization.source_content.bytes_hashed {
            return Err(AssetError::InvalidProvenance(
                "decode byte count differs from source fingerprint",
            ));
        }
        match &materialization.sample_rate {
            None => {
                if materialization.source_metadata.sample_rate_hz != output_metadata.sample_rate_hz
                    || materialization.source_metadata.channels != output_metadata.channels
                    || materialization.source_metadata.frame_count != output_metadata.frame_count
                {
                    return Err(AssetError::InvalidProvenance(
                        "unconverted material metadata differs from its source",
                    ));
                }
            }
            Some(recipe) => {
                if recipe.backend.trim().is_empty()
                    || recipe.backend_version.trim().is_empty()
                    || recipe.algorithm.trim().is_empty()
                    || recipe.interpolation.trim().is_empty()
                    || recipe.window.trim().is_empty()
                    || recipe.input_sample_rate_hz == 0
                    || recipe.output_sample_rate_hz == 0
                    || recipe.channels == 0
                    || recipe.input_frames == 0
                    || recipe.output_frames == 0
                    || recipe.chunk_frames == 0
                    || recipe.sinc_length == 0
                    || recipe.oversampling_factor == 0
                {
                    return Err(AssetError::InvalidProvenance(
                        "sample-rate materialization recipe is incomplete",
                    ));
                }
                if recipe.input_sample_rate_hz != materialization.source_metadata.sample_rate_hz
                    || recipe.channels != materialization.source_metadata.channels
                    || recipe.input_frames != materialization.source_metadata.frame_count.0
                    || recipe.output_sample_rate_hz != output_metadata.sample_rate_hz
                    || recipe.channels != output_metadata.channels
                    || recipe.output_frames != output_metadata.frame_count.0
                    || recipe.input_sample_rate_hz == recipe.output_sample_rate_hz
                {
                    return Err(AssetError::InvalidProvenance(
                        "sample-rate recipe does not connect source and output metadata",
                    ));
                }
            }
        }
        if output_content.bytes_hashed == 0 {
            return Err(AssetError::InvalidProvenance(
                "materialized PCM fingerprint is empty",
            ));
        }
        Ok(())
    }
}

/// The current resolver state for an asset. A relink preserves the old route
/// here *and* appends a permanent event to the record's relink history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetAvailability {
    Present,
    Missing {
        checked_at_unix_ms: u64,
    },
    Relinked {
        previous_location: AssetLocation,
        relinked_at_unix_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelinkEvent {
    pub previous_location: AssetLocation,
    pub new_location: AssetLocation,
    pub relinked_at_unix_ms: u64,
    pub basis: RelinkBasis,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelinkBasis {
    ExactContentFingerprint,
    MetadataAndNameCandidate,
    UserConfirmed,
}

/// An ordinary project entity using a source asset. These are intentionally
/// independent from UI module IDs so the asset manifest can survive editor
/// refactors and missing plugins.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AssetUsageOwner {
    AudioClip { persistent_id: u64 },
    SamplerZone { persistent_id: u64 },
    Step { persistent_id: u64 },
    AnalysisObject { persistent_id: u64 },
    Render { persistent_id: u64 },
    External { kind: String, persistent_id: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetUsage {
    pub id: AssetUsageId,
    pub owner: AssetUsageOwner,
    pub source_range: Option<AssetFrameRange>,
    pub label: String,
}

/// Registration input. The registry owns all generated IDs and preserves the
/// supplied metadata/content/provenance as immutable source facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetRegistration {
    pub name: String,
    pub location: AssetLocation,
    pub metadata: DecodedAudioMetadata,
    pub content: ContentFingerprint,
    pub provenance: AssetProvenance,
    pub tags: BTreeSet<String>,
    pub favorite: bool,
}

impl AssetRegistration {
    pub fn validate(&self) -> Result<(), AssetError> {
        if self.name.trim().is_empty() {
            return Err(AssetError::EmptyName);
        }
        self.location.validate()?;
        self.metadata.validate()?;
        if self.content.bytes_hashed == 0 {
            return Err(AssetError::InvalidFingerprint("zero bytes hashed"));
        }
        if self.provenance.original_location().validate().is_err() {
            return Err(AssetError::InvalidProvenance(
                "original location has no route",
            ));
        }
        self.provenance
            .validate_for_asset(&self.metadata, self.content)?;
        for tag in &self.tags {
            validate_tag(tag)?;
        }
        Ok(())
    }
}

/// A media-pool record. Content facts and provenance are exposed read-only;
/// only current presentation/location/use state is mutable through the
/// registry's checked operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaAsset {
    id: AssetId,
    name: String,
    location: AssetLocation,
    availability: AssetAvailability,
    metadata: DecodedAudioMetadata,
    content: ContentFingerprint,
    provenance: AssetProvenance,
    tags: BTreeSet<String>,
    favorite: bool,
    usages: BTreeMap<AssetUsageId, AssetUsage>,
    relink_history: Vec<RelinkEvent>,
}

impl MediaAsset {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_codec_parts(
        id: AssetId,
        name: String,
        location: AssetLocation,
        availability: AssetAvailability,
        metadata: DecodedAudioMetadata,
        content: ContentFingerprint,
        provenance: AssetProvenance,
        tags: BTreeSet<String>,
        favorite: bool,
        usages: Vec<AssetUsage>,
        relink_history: Vec<RelinkEvent>,
    ) -> Result<Self, String> {
        let mut usage_map = BTreeMap::new();
        for usage in usages {
            let usage_id = usage.id;
            if usage_map.insert(usage_id, usage).is_some() {
                return Err(format!("duplicate asset usage identity {}", usage_id.0));
            }
        }
        let asset = Self {
            id,
            name,
            location,
            availability,
            metadata,
            content,
            provenance,
            tags,
            favorite,
            usages: usage_map,
            relink_history,
        };
        let issues = asset.validate();
        if issues.is_empty() {
            Ok(asset)
        } else {
            Err(format!("invalid durable asset: {issues:?}"))
        }
    }

    pub fn id(&self) -> AssetId {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn location(&self) -> &AssetLocation {
        &self.location
    }
    pub fn availability(&self) -> &AssetAvailability {
        &self.availability
    }
    pub fn metadata(&self) -> &DecodedAudioMetadata {
        &self.metadata
    }
    pub fn content(&self) -> ContentFingerprint {
        self.content
    }
    pub fn provenance(&self) -> &AssetProvenance {
        &self.provenance
    }
    pub fn tags(&self) -> &BTreeSet<String> {
        &self.tags
    }
    pub fn is_favorite(&self) -> bool {
        self.favorite
    }

    /// Presentation-only copy used to build a put-style favorite command.
    /// Identity, provenance, usages, and source facts stay unchanged.
    pub fn with_favorite(&self, favorite: bool) -> Self {
        let mut next = self.clone();
        next.favorite = favorite;
        next
    }

    pub fn usages(&self) -> &BTreeMap<AssetUsageId, AssetUsage> {
        &self.usages
    }
    pub fn relink_history(&self) -> &[RelinkEvent] {
        &self.relink_history
    }

    /// Identity to verify when reopening or relinking the encoded source.
    /// For legacy/native assets this is the asset's own decoded metadata and
    /// byte fingerprint. Materialized imports retain those source facts in
    /// provenance while their public metadata/content describe renderable PCM.
    pub fn source_metadata(&self) -> &DecodedAudioMetadata {
        self.provenance
            .materialization()
            .map_or(&self.metadata, |materialization| {
                &materialization.source_metadata
            })
    }

    pub fn source_content(&self) -> ContentFingerprint {
        self.provenance
            .materialization()
            .map_or(self.content, |materialization| {
                materialization.source_content
            })
    }

    pub fn validate(&self) -> Vec<AssetValidationIssue> {
        let mut issues = Vec::new();
        if self.id.0 == 0 {
            issues.push(AssetValidationIssue::ZeroAssetId);
        }
        if self.name.trim().is_empty() {
            issues.push(AssetValidationIssue::EmptyName);
        }
        if let Err(error) = self.location.validate() {
            issues.push(AssetValidationIssue::Location(error));
        }
        if let Err(error) = self.metadata.validate() {
            issues.push(AssetValidationIssue::Metadata(error));
        }
        if self.content.bytes_hashed == 0 {
            issues.push(AssetValidationIssue::InvalidFingerprint);
        }
        if let Err(error) = self.provenance.original_location().validate() {
            issues.push(AssetValidationIssue::Provenance(error));
        }
        if let Err(error) = self
            .provenance
            .validate_for_asset(&self.metadata, self.content)
        {
            issues.push(AssetValidationIssue::Provenance(error));
        }
        for tag in &self.tags {
            if let Err(error) = validate_tag(tag) {
                issues.push(AssetValidationIssue::Tag(error));
            }
        }
        for (id, usage) in &self.usages {
            if *id != usage.id {
                issues.push(AssetValidationIssue::UsageKeyMismatch(*id));
            }
            if let Some(range) = usage.source_range {
                if !range.is_within(self.metadata.frame_count) {
                    issues.push(AssetValidationIssue::UsageOutsideAsset(*id, range));
                }
            }
        }
        issues
    }
}

/// A deterministic browser filter. Text matching is case-insensitive over the
/// name, tags, and current location label.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssetQuery {
    pub text: Option<String>,
    pub tags_all: BTreeSet<String>,
    pub favorite: Option<bool>,
    pub availability: Option<AssetAvailabilityKind>,
    pub minimum_frames: Option<SampleFrames>,
    pub maximum_frames: Option<SampleFrames>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AssetAvailabilityKind {
    Present,
    Missing,
    Relinked,
}

impl AssetAvailability {
    pub fn kind(&self) -> AssetAvailabilityKind {
        match self {
            Self::Present => AssetAvailabilityKind::Present,
            Self::Missing { .. } => AssetAvailabilityKind::Missing,
            Self::Relinked { .. } => AssetAvailabilityKind::Relinked,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AssetSort {
    #[default]
    NameAscending,
    NameDescending,
    FrameCountAscending,
    FrameCountDescending,
    IdAscending,
}

/// A location inspected by a relink scanner. It carries no promise that its
/// fingerprint is cryptographically strong; `fingerprint` is merely evidence
/// for deterministic ranking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelinkCandidate {
    pub location: AssetLocation,
    pub metadata: DecodedAudioMetadata,
    pub fingerprint: Option<ContentFingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScoredRelinkCandidate {
    pub candidate: RelinkCandidate,
    pub score: u32,
    pub reasons: Vec<RelinkScoreReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelinkScoreReason {
    ExactFingerprint,
    ExactFrameCount,
    ExactSampleRate,
    ExactChannelCount,
    MatchingFileName,
}

/// The media pool. A `BTreeMap` makes manifests, browser queries, duplicate
/// reports, and conflict diagnostics deterministic without depending on hash
/// iteration order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssetRegistry {
    assets: BTreeMap<AssetId, MediaAsset>,
    next_asset_id: u64,
    next_usage_id: u64,
}

impl AssetRegistry {
    pub fn new() -> Self {
        Self {
            assets: BTreeMap::new(),
            next_asset_id: 1,
            next_usage_id: 1,
        }
    }
    pub fn assets(&self) -> &BTreeMap<AssetId, MediaAsset> {
        &self.assets
    }
    pub fn get(&self, id: AssetId) -> Option<&MediaAsset> {
        self.assets.get(&id)
    }

    pub fn register(&mut self, registration: AssetRegistration) -> Result<AssetId, AssetError> {
        registration.validate()?;
        let id = self.allocate_asset_id()?;
        let tags = registration
            .tags
            .into_iter()
            .map(normalize_tag)
            .collect::<Result<_, _>>()?;
        let asset = MediaAsset {
            id,
            name: registration.name,
            location: registration.location,
            availability: AssetAvailability::Present,
            metadata: registration.metadata,
            content: registration.content,
            provenance: registration.provenance,
            tags,
            favorite: registration.favorite,
            usages: BTreeMap::new(),
            relink_history: Vec::new(),
        };
        self.assets.insert(id, asset);
        Ok(id)
    }

    /// Applies an exact put-style asset replacement without allocating an ID.
    ///
    /// `before` is an optimistic-concurrency guard. Supplying `None` creates
    /// or deletes in the usual put algebra; supplying two values replaces the
    /// complete checked record. Allocator cursors only advance, including
    /// when an older claimed identity is replayed after deletion.
    pub fn put_asset(
        &mut self,
        id: AssetId,
        before: Option<&MediaAsset>,
        after: Option<MediaAsset>,
    ) -> Result<(), AssetError> {
        if before.is_none() && after.is_none() {
            return Err(AssetError::EmptyPut("asset"));
        }
        if before.is_some_and(|asset| asset.id != id)
            || after.as_ref().is_some_and(|asset| asset.id != id)
        {
            return Err(AssetError::IdentityMismatch("asset"));
        }
        if self.assets.get(&id) != before {
            return Err(AssetError::StalePut("asset"));
        }
        match after {
            Some(asset) => {
                if !asset.validate().is_empty() {
                    return Err(AssetError::InvalidRecord("asset"));
                }
                let next_asset =
                    id.0.checked_add(1)
                        .ok_or(AssetError::IdExhausted("asset"))?;
                let next_usage = asset.usages.keys().next_back().map_or(Ok(1), |usage| {
                    usage
                        .0
                        .checked_add(1)
                        .ok_or(AssetError::IdExhausted("usage"))
                })?;
                self.next_asset_id = self.next_asset_id.max(next_asset);
                self.next_usage_id = self.next_usage_id.max(next_usage);
                self.assets.insert(id, asset);
            }
            None => {
                self.assets.remove(&id);
            }
        }
        Ok(())
    }

    /// Applies an exact put-style usage replacement inside one asset.
    pub fn put_usage(
        &mut self,
        asset: AssetId,
        id: AssetUsageId,
        before: Option<&AssetUsage>,
        after: Option<AssetUsage>,
    ) -> Result<(), AssetError> {
        if before.is_none() && after.is_none() {
            return Err(AssetError::EmptyPut("asset usage"));
        }
        if before.is_some_and(|usage| usage.id != id)
            || after.as_ref().is_some_and(|usage| usage.id != id)
        {
            return Err(AssetError::IdentityMismatch("asset usage"));
        }
        let entry = self.asset_mut(asset)?;
        if entry.usages.get(&id) != before {
            return Err(AssetError::StalePut("asset usage"));
        }
        if let Some(usage) = after.as_ref() {
            if let Some(range) = usage.source_range {
                if !range.is_within(entry.metadata.frame_count) {
                    return Err(AssetError::UsageOutsideAsset {
                        asset,
                        range,
                        frame_count: entry.metadata.frame_count,
                    });
                }
            }
        }
        match after {
            Some(usage) => {
                entry.usages.insert(id, usage);
                self.next_usage_id = self.next_usage_id.max(
                    id.0.checked_add(1)
                        .ok_or(AssetError::IdExhausted("usage"))?,
                );
            }
            None => {
                entry.usages.remove(&id);
            }
        }
        Ok(())
    }

    /// Applies an availability transition guarded by its exact prior value.
    pub fn put_availability(
        &mut self,
        asset: AssetId,
        before: &AssetAvailability,
        after: AssetAvailability,
    ) -> Result<(), AssetError> {
        let entry = self.asset_mut(asset)?;
        if &entry.availability != before {
            return Err(AssetError::StalePut("asset availability"));
        }
        entry.availability = after;
        Ok(())
    }

    pub fn set_name(&mut self, id: AssetId, name: impl Into<String>) -> Result<(), AssetError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(AssetError::EmptyName);
        }
        self.asset_mut(id)?.name = name;
        Ok(())
    }

    pub fn set_favorite(&mut self, id: AssetId, favorite: bool) -> Result<(), AssetError> {
        self.asset_mut(id)?.favorite = favorite;
        Ok(())
    }

    pub fn add_tag(&mut self, id: AssetId, tag: impl Into<String>) -> Result<(), AssetError> {
        self.asset_mut(id)?.tags.insert(normalize_tag(tag.into())?);
        Ok(())
    }

    pub fn remove_tag(&mut self, id: AssetId, tag: &str) -> Result<bool, AssetError> {
        Ok(self
            .asset_mut(id)?
            .tags
            .remove(&normalize_tag(tag.to_owned())?))
    }

    pub fn mark_missing(&mut self, id: AssetId, checked_at_unix_ms: u64) -> Result<(), AssetError> {
        self.asset_mut(id)?.availability = AssetAvailability::Missing { checked_at_unix_ms };
        Ok(())
    }

    pub fn mark_present(&mut self, id: AssetId) -> Result<(), AssetError> {
        self.asset_mut(id)?.availability = AssetAvailability::Present;
        Ok(())
    }

    pub fn add_usage(
        &mut self,
        asset: AssetId,
        owner: AssetUsageOwner,
        source_range: Option<AssetFrameRange>,
        label: impl Into<String>,
    ) -> Result<AssetUsageId, AssetError> {
        let usage_id = self.allocate_usage_id()?;
        let entry = self.asset_mut(asset)?;
        if let Some(range) = source_range {
            if !range.is_within(entry.metadata.frame_count) {
                return Err(AssetError::UsageOutsideAsset {
                    asset,
                    range,
                    frame_count: entry.metadata.frame_count,
                });
            }
        }
        entry.usages.insert(
            usage_id,
            AssetUsage {
                id: usage_id,
                owner,
                source_range,
                label: label.into(),
            },
        );
        Ok(usage_id)
    }

    pub fn remove_usage(
        &mut self,
        asset: AssetId,
        usage: AssetUsageId,
    ) -> Result<Option<AssetUsage>, AssetError> {
        Ok(self.asset_mut(asset)?.usages.remove(&usage))
    }

    /// Relinking does not alter content metadata or import provenance. Use
    /// [`Self::rank_relink_candidates`] first; callers must explicitly choose
    /// the candidate they are willing to accept.
    pub fn relink(
        &mut self,
        id: AssetId,
        location: AssetLocation,
        relinked_at_unix_ms: u64,
        basis: RelinkBasis,
    ) -> Result<(), AssetError> {
        location.validate()?;
        let asset = self.asset_mut(id)?;
        let previous_location = asset.location.clone();
        asset.location = location.clone();
        asset.availability = AssetAvailability::Relinked {
            previous_location: previous_location.clone(),
            relinked_at_unix_ms,
        };
        asset.relink_history.push(RelinkEvent {
            previous_location,
            new_location: location,
            relinked_at_unix_ms,
            basis,
        });
        Ok(())
    }

    pub fn search(&self, query: &AssetQuery, sort: AssetSort) -> Result<Vec<AssetId>, AssetError> {
        for tag in &query.tags_all {
            validate_tag(tag)?;
        }
        if let (Some(minimum), Some(maximum)) = (query.minimum_frames, query.maximum_frames) {
            if minimum > maximum {
                return Err(AssetError::InvalidQuery(
                    "minimum frames exceeds maximum frames",
                ));
            }
        }
        let normalized_tags = query
            .tags_all
            .iter()
            .map(|tag| normalize_tag(tag.clone()))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let needle = query
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_lowercase);
        let mut found = self
            .assets
            .values()
            .filter(|asset| {
                let text_matches = needle
                    .as_ref()
                    .is_none_or(|needle| searchable_text(asset).contains(needle));
                let tags_match = normalized_tags.iter().all(|tag| asset.tags.contains(tag));
                let favorite_matches = query
                    .favorite
                    .is_none_or(|favorite| asset.favorite == favorite);
                let availability_matches = query
                    .availability
                    .is_none_or(|kind| asset.availability.kind() == kind);
                let min_matches = query
                    .minimum_frames
                    .is_none_or(|minimum| asset.metadata.frame_count >= minimum);
                let max_matches = query
                    .maximum_frames
                    .is_none_or(|maximum| asset.metadata.frame_count <= maximum);
                text_matches
                    && tags_match
                    && favorite_matches
                    && availability_matches
                    && min_matches
                    && max_matches
            })
            .collect::<Vec<_>>();
        found.sort_by(|left, right| compare_assets(left, right, sort));
        Ok(found.into_iter().map(|asset| asset.id).collect())
    }

    /// Content-equal groups. Because FNV-1a is non-cryptographic, callers who
    /// plan to discard data must compare source bytes before taking action.
    pub fn duplicate_groups(&self) -> Vec<DuplicateGroup> {
        let mut grouped: BTreeMap<ContentFingerprint, Vec<AssetId>> = BTreeMap::new();
        for asset in self.assets.values() {
            grouped.entry(asset.content).or_default().push(asset.id);
        }
        grouped
            .into_iter()
            .filter_map(|(fingerprint, mut assets)| {
                (assets.len() > 1).then(|| {
                    assets.sort();
                    DuplicateGroup {
                        fingerprint,
                        assets,
                    }
                })
            })
            .collect()
    }

    /// Score scanner results without modifying the project. An exact matching
    /// fingerprint is overwhelming evidence but remains labeled as a
    /// non-cryptographic duplicate hint in the public types.
    pub fn rank_relink_candidates(
        &self,
        id: AssetId,
        candidates: &[RelinkCandidate],
    ) -> Result<Vec<ScoredRelinkCandidate>, AssetError> {
        let asset = self.get(id).ok_or(AssetError::UnknownAsset(id))?;
        let mut scored = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            candidate.location.validate()?;
            candidate.metadata.validate()?;
            let mut score = 0;
            let mut reasons = Vec::new();
            if candidate.fingerprint == Some(asset.source_content()) {
                score += 100_000;
                reasons.push(RelinkScoreReason::ExactFingerprint);
            }
            if candidate.metadata.frame_count == asset.source_metadata().frame_count {
                score += 1_000;
                reasons.push(RelinkScoreReason::ExactFrameCount);
            }
            if candidate.metadata.sample_rate_hz == asset.source_metadata().sample_rate_hz {
                score += 100;
                reasons.push(RelinkScoreReason::ExactSampleRate);
            }
            if candidate.metadata.channels == asset.source_metadata().channels {
                score += 50;
                reasons.push(RelinkScoreReason::ExactChannelCount);
            }
            if file_name(&candidate.location) == file_name(&asset.location) {
                score += 25;
                reasons.push(RelinkScoreReason::MatchingFileName);
            }
            scored.push(ScoredRelinkCandidate {
                candidate: candidate.clone(),
                score,
                reasons,
            });
        }
        scored.sort_by(|left, right| {
            right.score.cmp(&left.score).then_with(|| {
                left.candidate
                    .location
                    .stable_label()
                    .cmp(&right.candidate.location.stable_label())
            })
        });
        Ok(scored)
    }

    pub fn validate(&self) -> Vec<RegistryValidationIssue> {
        let mut issues = Vec::new();
        let max_asset = self.assets.keys().last().map_or(0, |id| id.0);
        if self.next_asset_id <= max_asset {
            issues.push(RegistryValidationIssue::AssetAllocatorNotAhead {
                next: self.next_asset_id,
                max: max_asset,
            });
        }
        let mut maximum_usage = 0;
        for asset in self.assets.values() {
            for issue in asset.validate() {
                issues.push(RegistryValidationIssue::Asset {
                    asset: asset.id,
                    issue,
                });
            }
            maximum_usage = maximum_usage.max(asset.usages.keys().last().map_or(0, |id| id.0));
        }
        if self.next_usage_id <= maximum_usage {
            issues.push(RegistryValidationIssue::UsageAllocatorNotAhead {
                next: self.next_usage_id,
                max: maximum_usage,
            });
        }
        issues
    }

    fn asset_mut(&mut self, id: AssetId) -> Result<&mut MediaAsset, AssetError> {
        self.assets.get_mut(&id).ok_or(AssetError::UnknownAsset(id))
    }

    fn allocate_asset_id(&mut self) -> Result<AssetId, AssetError> {
        let id = self.next_asset_id;
        self.next_asset_id = self
            .next_asset_id
            .checked_add(1)
            .ok_or(AssetError::IdExhausted("asset"))?;
        Ok(AssetId(id))
    }

    fn allocate_usage_id(&mut self) -> Result<AssetUsageId, AssetError> {
        let id = self.next_usage_id;
        self.next_usage_id = self
            .next_usage_id
            .checked_add(1)
            .ok_or(AssetError::IdExhausted("usage"))?;
        Ok(AssetUsageId(id))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateGroup {
    pub fingerprint: ContentFingerprint,
    pub assets: Vec<AssetId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetValidationIssue {
    ZeroAssetId,
    EmptyName,
    Location(AssetError),
    Metadata(AssetError),
    InvalidFingerprint,
    Provenance(AssetError),
    Tag(AssetError),
    UsageKeyMismatch(AssetUsageId),
    UsageOutsideAsset(AssetUsageId, AssetFrameRange),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryValidationIssue {
    AssetAllocatorNotAhead {
        next: u64,
        max: u64,
    },
    UsageAllocatorNotAhead {
        next: u64,
        max: u64,
    },
    Asset {
        asset: AssetId,
        issue: AssetValidationIssue,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetError {
    PathMustBeAbsolute(String),
    InvalidProjectRelativePath(String),
    LocationHasNoRoute,
    InvalidAudioMetadata(&'static str),
    InvalidFrameRange {
        start: SampleFrames,
        end: SampleFrames,
    },
    InvalidFingerprint(&'static str),
    InvalidProvenance(&'static str),
    EmptyName,
    InvalidTag(String),
    UnknownAsset(AssetId),
    UsageOutsideAsset {
        asset: AssetId,
        range: AssetFrameRange,
        frame_count: SampleFrames,
    },
    InvalidQuery(&'static str),
    EmptyPut(&'static str),
    IdentityMismatch(&'static str),
    StalePut(&'static str),
    InvalidRecord(&'static str),
    IdExhausted(&'static str),
}

impl fmt::Display for AssetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathMustBeAbsolute(path) => write!(formatter, "path is not absolute: {path}"),
            Self::InvalidProjectRelativePath(path) => {
                write!(formatter, "invalid project-relative path: {path}")
            }
            Self::LocationHasNoRoute => write!(formatter, "asset location has no route"),
            Self::InvalidAudioMetadata(reason) => {
                write!(formatter, "invalid decoded audio metadata: {reason}")
            }
            Self::InvalidFrameRange { start, end } => write!(
                formatter,
                "invalid half-open frame range {}..{}",
                start.0, end.0
            ),
            Self::InvalidFingerprint(reason) => {
                write!(formatter, "invalid content fingerprint: {reason}")
            }
            Self::InvalidProvenance(reason) => {
                write!(formatter, "invalid asset provenance: {reason}")
            }
            Self::EmptyName => write!(formatter, "asset name is empty"),
            Self::InvalidTag(tag) => write!(formatter, "invalid tag: {tag:?}"),
            Self::UnknownAsset(id) => write!(formatter, "unknown asset {}", id.0),
            Self::UsageOutsideAsset {
                asset,
                range,
                frame_count,
            } => write!(
                formatter,
                "usage {}..{} lies outside asset {} ({} frames)",
                range.start.0, range.end.0, asset.0, frame_count.0
            ),
            Self::InvalidQuery(reason) => write!(formatter, "invalid asset query: {reason}"),
            Self::EmptyPut(kind) => write!(formatter, "empty {kind} put"),
            Self::IdentityMismatch(kind) => write!(formatter, "{kind} put identity mismatch"),
            Self::StalePut(kind) => write!(formatter, "stale {kind} put"),
            Self::InvalidRecord(kind) => write!(formatter, "invalid {kind} record"),
            Self::IdExhausted(kind) => write!(formatter, "{kind} ID space exhausted"),
        }
    }
}

impl Error for AssetError {}

fn validate_tag(tag: &str) -> Result<(), AssetError> {
    let tag = tag.trim();
    if tag.is_empty() || tag.len() > 128 || tag.chars().any(char::is_control) {
        return Err(AssetError::InvalidTag(tag.to_owned()));
    }
    Ok(())
}

fn normalize_tag(tag: String) -> Result<String, AssetError> {
    validate_tag(&tag)?;
    Ok(tag.trim().to_lowercase())
}

fn searchable_text(asset: &MediaAsset) -> String {
    let mut text = asset.name.to_lowercase();
    text.push(' ');
    text.push_str(&asset.location.stable_label().to_lowercase());
    for tag in &asset.tags {
        text.push(' ');
        text.push_str(tag);
    }
    text
}

fn compare_assets(left: &MediaAsset, right: &MediaAsset, sort: AssetSort) -> Ordering {
    let name = || {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    };
    match sort {
        AssetSort::NameAscending => name(),
        AssetSort::NameDescending => name().reverse(),
        AssetSort::FrameCountAscending => left
            .metadata
            .frame_count
            .cmp(&right.metadata.frame_count)
            .then_with(|| name()),
        AssetSort::FrameCountDescending => right
            .metadata
            .frame_count
            .cmp(&left.metadata.frame_count)
            .then_with(|| name()),
        AssetSort::IdAscending => left.id.cmp(&right.id),
    }
}

fn file_name(location: &AssetLocation) -> String {
    let raw = location.stable_label();
    Path::new(&raw)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(frames: u64) -> DecodedAudioMetadata {
        DecodedAudioMetadata {
            sample_rate_hz: 48_000,
            channels: 2,
            frame_count: SampleFrames(frames),
            container: Some("flac".into()),
            codec: Some("flac".into()),
            bit_depth: Some(24),
        }
    }

    fn location(name: &str) -> AssetLocation {
        AssetLocation::new(
            Some(AbsolutePath::parse(format!("/music/{name}")).unwrap()),
            Some(ProjectRelativePath::parse(format!("media/{name}")).unwrap()),
        )
        .unwrap()
    }

    fn registration(name: &str, bytes: &[u8], frames: u64) -> AssetRegistration {
        AssetRegistration {
            name: name.into(),
            location: location(name),
            metadata: metadata(frames),
            content: ContentFingerprint::from_bytes(bytes),
            provenance: AssetProvenance::new(
                7,
                AssetOrigin::ImportedFile {
                    importer: "test".into(),
                },
                location(name),
            ),
            tags: BTreeSet::from(["Cold".into(), "Percussive".into()]),
            favorite: false,
        }
    }

    #[test]
    fn content_id_is_stable_and_explicitly_non_cryptographic() {
        let first = ContentFingerprint::from_bytes(b"silent shout");
        assert_eq!(first, ContentFingerprint::from_bytes(b"silent shout"));
        assert_ne!(first, ContentFingerprint::from_bytes(b"Silent Shout"));
        assert_eq!(
            first.algorithm,
            ContentHashAlgorithm::Fnv1a128NonCryptographic
        );
        assert_eq!(first.id.to_hex().len(), 32);
    }

    #[test]
    fn paths_preserve_absolute_and_portable_routes() {
        assert!(AbsolutePath::parse("media/a.wav").is_err());
        assert!(ProjectRelativePath::parse("../outside.wav").is_err());
        let location = location("a.wav");
        assert_eq!(
            location.candidate_paths(Some(Path::new("/project"))),
            vec![
                PathBuf::from("/project/media/a.wav"),
                PathBuf::from("/music/a.wav")
            ]
        );
    }

    #[test]
    fn decoded_metadata_and_half_open_ranges_are_exact() {
        assert!(metadata(0).validate().is_err());
        let range = AssetFrameRange::new(SampleFrames(2), SampleFrames(8)).unwrap();
        assert_eq!(range.len(), SampleFrames(6));
        assert!(range.is_within(SampleFrames(8)));
        assert!(!range.is_within(SampleFrames(7)));
    }

    #[test]
    fn registry_allocates_never_reused_asset_and_usage_ids() {
        let mut pool = AssetRegistry::new();
        let first = pool.register(registration("a.flac", b"same", 100)).unwrap();
        let second = pool
            .register(registration("b.flac", b"other", 100))
            .unwrap();
        assert_eq!((first, second), (AssetId(1), AssetId(2)));
        let usage = pool
            .add_usage(
                first,
                AssetUsageOwner::AudioClip { persistent_id: 4 },
                Some(AssetFrameRange::new(SampleFrames(0), SampleFrames(100)).unwrap()),
                "full clip",
            )
            .unwrap();
        assert_eq!(usage, AssetUsageId(1));
        assert!(pool.remove_usage(first, usage).unwrap().is_some());
        let replacement = pool
            .add_usage(
                first,
                AssetUsageOwner::Step { persistent_id: 5 },
                None,
                "hit",
            )
            .unwrap();
        assert_eq!(replacement, AssetUsageId(2));
        assert!(pool.validate().is_empty());
    }

    #[test]
    fn exact_puts_are_guarded_and_never_rewind_allocators() {
        let mut pool = AssetRegistry::new();
        let id = pool.register(registration("a.flac", b"a", 100)).unwrap();
        let original = pool.get(id).unwrap().clone();

        pool.put_asset(id, Some(&original), None).unwrap();
        assert!(matches!(
            pool.put_asset(id, Some(&original), None),
            Err(AssetError::StalePut("asset"))
        ));
        pool.put_asset(id, None, Some(original.clone())).unwrap();
        let next = pool.register(registration("b.flac", b"b", 100)).unwrap();
        assert_eq!(next, AssetId(2));

        let usage = AssetUsage {
            id: AssetUsageId(9),
            owner: AssetUsageOwner::SamplerZone { persistent_id: 4 },
            source_range: Some(AssetFrameRange::new(SampleFrames(2), SampleFrames(8)).unwrap()),
            label: "slice".into(),
        };
        pool.put_usage(id, usage.id, None, Some(usage.clone()))
            .unwrap();
        assert!(matches!(
            pool.put_usage(id, usage.id, None, Some(usage)),
            Err(AssetError::StalePut("asset usage"))
        ));
        let allocated = pool
            .add_usage(id, AssetUsageOwner::Step { persistent_id: 8 }, None, "next")
            .unwrap();
        assert_eq!(allocated, AssetUsageId(10));
        assert!(pool.validate().is_empty());
    }

    #[test]
    fn out_of_range_usage_is_rejected() {
        let mut pool = AssetRegistry::new();
        let id = pool.register(registration("a.flac", b"x", 100)).unwrap();
        let error = pool
            .add_usage(
                id,
                AssetUsageOwner::AudioClip { persistent_id: 1 },
                Some(AssetFrameRange::new(SampleFrames(99), SampleFrames(101)).unwrap()),
                "bad",
            )
            .unwrap_err();
        assert!(matches!(error, AssetError::UsageOutsideAsset { .. }));
    }

    #[test]
    fn favorite_put_preserves_identity_and_flips_only_the_star() {
        let mut pool = AssetRegistry::new();
        let id = pool.register(registration("a.flac", b"a", 100)).unwrap();
        let before = pool.get(id).unwrap().clone();
        let after = before.with_favorite(true);
        assert_eq!(after.id(), before.id());
        assert_eq!(after.name(), before.name());
        assert_eq!(after.content(), before.content());
        assert!(after.is_favorite());
        assert!(!before.is_favorite());
        pool.put_asset(id, Some(&before), Some(after)).unwrap();
        assert!(pool.get(id).unwrap().is_favorite());
        assert_eq!(pool.get(id).unwrap().id(), id);
    }

    #[test]
    fn tags_search_and_sort_are_canonical_and_deterministic() {
        let mut pool = AssetRegistry::new();
        let a = pool
            .register(registration("Zebra.flac", b"z", 200))
            .unwrap();
        let b = pool
            .register(registration("alpha.flac", b"a", 100))
            .unwrap();
        pool.set_favorite(a, true).unwrap();
        pool.add_tag(b, "Acid").unwrap();
        let query = AssetQuery {
            text: Some("MEDIA".into()),
            tags_all: BTreeSet::from(["COLD".into()]),
            favorite: None,
            availability: None,
            minimum_frames: None,
            maximum_frames: None,
        };
        assert_eq!(
            pool.search(&query, AssetSort::NameAscending).unwrap(),
            vec![b, a]
        );
        assert_eq!(
            pool.search(
                &AssetQuery {
                    favorite: Some(true),
                    ..AssetQuery::default()
                },
                AssetSort::IdAscending
            )
            .unwrap(),
            vec![a]
        );
        assert!(pool.get(b).unwrap().tags().contains("acid"));
    }

    #[test]
    fn duplicate_groups_are_content_based_and_stably_ordered() {
        let mut pool = AssetRegistry::new();
        let a = pool
            .register(registration("one.flac", b"same", 10))
            .unwrap();
        let b = pool
            .register(registration("two.flac", b"same", 10))
            .unwrap();
        let _ = pool
            .register(registration("three.flac", b"different", 10))
            .unwrap();
        let groups = pool.duplicate_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].assets, vec![a, b]);
        assert_eq!(
            groups[0].fingerprint.algorithm,
            ContentHashAlgorithm::Fnv1a128NonCryptographic
        );
    }

    #[test]
    fn relinking_preserves_immutable_import_provenance_and_history() {
        let mut pool = AssetRegistry::new();
        let id = pool
            .register(registration("gone.flac", b"bytes", 10))
            .unwrap();
        let original = pool.get(id).unwrap().provenance().clone();
        pool.mark_missing(id, 20).unwrap();
        pool.relink(
            id,
            location("found.flac"),
            30,
            RelinkBasis::ExactContentFingerprint,
        )
        .unwrap();
        let asset = pool.get(id).unwrap();
        assert_eq!(asset.provenance(), &original);
        assert!(matches!(
            asset.availability(),
            AssetAvailability::Relinked {
                relinked_at_unix_ms: 30,
                ..
            }
        ));
        assert_eq!(asset.relink_history().len(), 1);
        assert_eq!(
            asset.relink_history()[0].previous_location,
            location("gone.flac")
        );
    }

    #[test]
    fn relink_ranking_prefers_exact_content_then_uses_deterministic_tiebreaks() {
        let mut pool = AssetRegistry::new();
        let id = pool
            .register(registration("kick.flac", b"exact", 400))
            .unwrap();
        let exact = RelinkCandidate {
            location: location("z/kick.flac"),
            metadata: metadata(400),
            fingerprint: Some(ContentFingerprint::from_bytes(b"exact")),
        };
        let metadata_only_a = RelinkCandidate {
            location: location("a/kick.flac"),
            metadata: metadata(400),
            fingerprint: None,
        };
        let metadata_only_b = RelinkCandidate {
            location: location("b/kick.flac"),
            metadata: metadata(400),
            fingerprint: None,
        };
        let ranked = pool
            .rank_relink_candidates(id, &[metadata_only_b, exact.clone(), metadata_only_a])
            .unwrap();
        assert_eq!(ranked[0].candidate, exact);
        assert!(ranked[0]
            .reasons
            .contains(&RelinkScoreReason::ExactFingerprint));
        assert!(
            ranked[1].candidate.location.stable_label()
                < ranked[2].candidate.location.stable_label()
        );
    }

    #[test]
    fn materialized_asset_relink_scores_the_encoded_source_identity() {
        let source_metadata = DecodedAudioMetadata {
            sample_rate_hz: 44_100,
            channels: 1,
            frame_count: SampleFrames(441),
            container: Some("wav".into()),
            codec: Some("pcm_s16le".into()),
            bit_depth: Some(16),
        };
        let source_content = ContentFingerprint::from_bytes(b"encoded source");
        let mut materialized = registration("moved.wav", b"canonical project PCM", 480);
        materialized.metadata.sample_rate_hz = 48_000;
        materialized.metadata.channels = 1;
        materialized.metadata.container = Some("audec-canonical-pcm".into());
        materialized.metadata.codec = Some("pcm_f32le".into());
        materialized.metadata.bit_depth = Some(32);
        materialized.provenance =
            materialized
                .provenance
                .with_materialization(PcmMaterializationProvenance {
                    source_metadata: source_metadata.clone(),
                    source_content,
                    decode: SourceDecodeProvenance {
                        backend: "symphonia".into(),
                        backend_version: "0.5.5".into(),
                        source_bytes: source_content.bytes_hashed,
                        stream_count: 1,
                        selected_track_id: 0,
                        container: Some("wav".into()),
                        codec: "pcm_s16le".into(),
                        declared_frames: Some(441),
                        gapless: true,
                        verification: DecodeIntegrity::Unavailable,
                    },
                    sample_rate: Some(SampleRateMaterializationRecipe {
                        backend: "rubato".into(),
                        backend_version: "5.0.0".into(),
                        algorithm: "asynchronous-windowed-sinc".into(),
                        input_sample_rate_hz: 44_100,
                        output_sample_rate_hz: 48_000,
                        channels: 1,
                        input_frames: 441,
                        output_frames: 480,
                        chunk_frames: 1_024,
                        sinc_length: 256,
                        cutoff_bits: None,
                        oversampling_factor: 128,
                        interpolation: "cubic".into(),
                        window: "blackman-harris2".into(),
                        delay_removed: true,
                        trimmed_output_frames: 0,
                    }),
                });
        let mut pool = AssetRegistry::new();
        let asset = pool.register(materialized).unwrap();
        let encoded_candidate = RelinkCandidate {
            location: location("encoded-source.wav"),
            metadata: source_metadata,
            fingerprint: Some(source_content),
        };
        let output_candidate = RelinkCandidate {
            location: location("render-cache.wav"),
            metadata: pool.get(asset).unwrap().metadata().clone(),
            fingerprint: Some(pool.get(asset).unwrap().content()),
        };

        let ranked = pool
            .rank_relink_candidates(asset, &[output_candidate, encoded_candidate.clone()])
            .unwrap();
        assert_eq!(ranked[0].candidate, encoded_candidate);
        assert!(ranked[0]
            .reasons
            .contains(&RelinkScoreReason::ExactFingerprint));
        assert!(!ranked[1]
            .reasons
            .contains(&RelinkScoreReason::ExactFingerprint));
    }

    #[test]
    fn invalid_manifest_state_is_reported_without_panicking() {
        let mut pool = AssetRegistry::new();
        let id = pool.register(registration("a.flac", b"x", 5)).unwrap();
        // Private facts cannot be edited through the public API, but public
        // validation still catches allocator corruption after a malformed
        // deserialize implementation constructs this registry.
        pool.next_asset_id = id.0;
        assert!(matches!(
            pool.validate()[0],
            RegistryValidationIssue::AssetAllocatorNotAhead { .. }
        ));
    }
}
