//! Portable, crash-resilient project-file I/O for the constructive audec DAW.
//!
//! This is deliberately an envelope, rather than a serializer for live Rust
//! editor objects.  Each DAW domain owns its own versioned payload, while this
//! module owns the durable facts every project must share: project identity,
//! revision, asset-routing intent, workspace state, extension payloads, and
//! recovery provenance.  That keeps an arrangement or plugin schema change
//! from silently changing the on-disk meaning of a project as a whole.
//!
//! Files are UTF-8 JSON on purpose: a broken or future project can be
//! inspected, diffed, and repaired by a human.  Atomic writes use a sibling
//! temporary file, `sync_all`, and rename; a sibling autosave is kept separate
//! from the last known-good primary project.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::assets::{
    AssetAvailability, AssetLocation, AssetOrigin, DecodeIntegrity, MediaAsset,
    PcmMaterializationProvenance,
};
use crate::content_identity::{ContentClass, Digest, SchemaTag, Sha256Digest};
use crate::content_store::ObjectRef;
use crate::daw_project::{DawProject, ProjectDomain, ProjectSaveIntent};
use crate::workspace::WorkspaceSnapshotDto;
use crate::workspace_document::WorkspaceDocument;

pub const PROJECT_FILE_FORMAT: &str = "audec-project";
pub const PROJECT_FILE_VERSION: u32 = 1;
/// Stable extension key for the dynamic, GPUI-free workspace document.
///
/// It deliberately lives in the envelope extension map rather than changing
/// the v1 fixed field set: old v1 readers retain it byte-for-byte, while new
/// readers can prefer it over the legacy six-view snapshot.
pub const WORKSPACE_DOCUMENT_EXTENSION_KEY: &str = "audec.workspace_document.v2";
/// Stable, generic roots into the schema-tagged content store. Product
/// modules persist their own versioned receipt objects; the project envelope
/// only keeps typed roots and therefore need not learn every product codec.
pub const CONTENT_ROOTS_EXTENSION_KEY: &str = "audec.content_roots.v1";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const CONTENT_ROOTS_FORMAT: &str = "audec-content-roots";
const CONTENT_ROOTS_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentObjectRecord {
    pub schema_class: String,
    pub schema_name: String,
    pub schema_version: u32,
    pub algorithm: String,
    pub sha256: String,
    pub byte_len: u64,
}

impl ContentObjectRecord {
    pub fn from_object(object: &ObjectRef) -> Self {
        Self {
            schema_class: object.digest.schema().class().storage_label().into(),
            schema_name: object.digest.schema().name().into(),
            schema_version: object.digest.schema().version(),
            algorithm: Digest::ALGORITHM.into(),
            sha256: object.digest.sha256().to_hex(),
            byte_len: object.byte_len,
        }
    }

    pub fn object_ref(&self) -> Result<ObjectRef, ProjectIoError> {
        if self.algorithm != Digest::ALGORITHM {
            return Err(ProjectIoError::Invalid(format!(
                "unsupported content-root digest algorithm {}",
                self.algorithm
            )));
        }
        let class = ContentClass::from_storage_label(&self.schema_class).ok_or_else(|| {
            ProjectIoError::Invalid(format!(
                "unknown content-root schema class {}",
                self.schema_class
            ))
        })?;
        let schema = SchemaTag::new(class, self.schema_name.clone(), self.schema_version)
            .map_err(|error| ProjectIoError::Invalid(error.to_string()))?;
        let sha256 = Sha256Digest::from_hex(&self.sha256)
            .map_err(|error| ProjectIoError::Invalid(error.to_string()))?;
        Ok(ObjectRef {
            digest: Digest::from_verified_parts(schema, sha256),
            byte_len: self.byte_len,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRootRecord {
    /// Product family, e.g. `render-product` or `analysis-artifact`.
    pub role: String,
    /// Stable identity within the owning project/product domain.
    pub logical_id: String,
    /// CAS object containing the product module's versioned receipt manifest.
    pub receipt: ContentObjectRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectContentRoots {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub roots: Vec<ContentRootRecord>,
}

impl Default for ProjectContentRoots {
    fn default() -> Self {
        Self {
            format: CONTENT_ROOTS_FORMAT.into(),
            version: CONTENT_ROOTS_VERSION,
            roots: Vec::new(),
        }
    }
}

impl ProjectContentRoots {
    pub fn validate(&self) -> Result<(), ProjectIoError> {
        if self.format != CONTENT_ROOTS_FORMAT || self.version != CONTENT_ROOTS_VERSION {
            return Err(ProjectIoError::Invalid(format!(
                "unsupported content-root manifest {}@{}",
                self.format, self.version
            )));
        }
        let mut identities = BTreeSet::new();
        for root in &self.roots {
            if !valid_content_root_label(&root.role) || root.logical_id.trim().is_empty() {
                return Err(ProjectIoError::Invalid(
                    "content-root role and logical identity must be non-empty".into(),
                ));
            }
            if !identities.insert((&root.role, &root.logical_id)) {
                return Err(ProjectIoError::Invalid(format!(
                    "duplicate content root {}/{}",
                    root.role, root.logical_id
                )));
            }
            root.receipt.object_ref()?;
        }
        Ok(())
    }
}

/// A path route persisted as an *intent*, never as an assertion that a file
/// exists.  Project-relative routes are preferred when reopening elsewhere;
/// an original absolute route remains an explicit relink hint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetPathIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_relative: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_absolute: Option<PathBuf>,
}

impl AssetPathIntent {
    pub fn from_location(location: &AssetLocation) -> Self {
        Self {
            project_relative: location
                .project_relative
                .as_ref()
                .map(|path| PathBuf::from(path.as_str())),
            original_absolute: location
                .absolute
                .as_ref()
                .map(|path| path.as_path().to_path_buf()),
        }
    }

    /// Construct portable routing for a newly observed filesystem path.
    pub fn from_observed_path(project_file: &Path, observed: &Path) -> Self {
        let root = project_file.parent().unwrap_or_else(|| Path::new("."));
        let project_relative = observed
            .strip_prefix(root)
            .ok()
            .filter(|path| safe_relative_path(path))
            .map(Path::to_path_buf);
        Self {
            project_relative,
            original_absolute: observed.is_absolute().then(|| observed.to_path_buf()),
        }
    }

    /// Candidate paths in deterministic portability-first order.  This does
    /// not touch the filesystem, so callers may show missing-media UI without
    /// blocking a UI thread.
    pub fn candidates(&self, project_file: &Path) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        if let Some(relative) = &self.project_relative {
            if safe_relative_path(relative) {
                let root = project_file.parent().unwrap_or_else(|| Path::new("."));
                candidates.push(root.join(relative));
            }
        }
        if let Some(absolute) = &self.original_absolute {
            if !candidates.contains(absolute) {
                candidates.push(absolute.clone());
            }
        }
        candidates
    }

    pub fn validate(&self) -> Result<(), ProjectIoError> {
        if self.project_relative.is_none() && self.original_absolute.is_none() {
            return Err(ProjectIoError::Invalid(
                "an asset path intent has no relative or original route".into(),
            ));
        }
        if let Some(relative) = &self.project_relative {
            if !safe_relative_path(relative) {
                return Err(ProjectIoError::Invalid(format!(
                    "asset route escapes project root: {}",
                    relative.display()
                )));
            }
        }
        if let Some(absolute) = &self.original_absolute {
            if !absolute.is_absolute() {
                return Err(ProjectIoError::Invalid(format!(
                    "original asset route is not absolute: {}",
                    absolute.display()
                )));
            }
        }
        Ok(())
    }
}

/// Persisted description of a media-pool entry.  It is intentionally a
/// relink manifest, not embedded audio; audio may be copied into `media/` by
/// a future portable-package command without changing this schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRecord {
    pub id: u64,
    pub name: String,
    pub path: AssetPathIntent,
    pub content_algorithm: String,
    pub content_id: String,
    pub bytes_hashed: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frame_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u16>,
    pub origin: String,
    pub availability: String,
    #[serde(default)]
    pub tags: BTreeSet<String>,
    #[serde(default)]
    pub favorite: bool,
    /// Present for imports whose reusable canonical PCM is distinct from the
    /// encoded source identity used for reopen/relink verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialization: Option<AssetMaterializationRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetMaterializationRecord {
    pub source_content_id: String,
    pub source_bytes_hashed: u64,
    pub source_sample_rate_hz: u32,
    pub source_channels: u16,
    pub source_frame_count: u64,
    pub decoder: String,
    pub decoder_version: String,
    pub selected_track_id: u32,
    pub decode_verification: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_recipe: Option<AssetSampleRateRecipeRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetSampleRateRecipeRecord {
    pub backend: String,
    pub backend_version: String,
    pub algorithm: String,
    pub input_sample_rate_hz: u32,
    pub output_sample_rate_hz: u32,
    pub channels: u16,
    pub input_frames: u64,
    pub output_frames: u64,
    pub chunk_frames: u64,
    pub sinc_length: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff_bits: Option<u32>,
    pub oversampling_factor: u64,
    pub interpolation: String,
    pub window: String,
    pub delay_removed: bool,
    pub trimmed_output_frames: u64,
}

impl AssetMaterializationRecord {
    fn from_model(value: &PcmMaterializationProvenance) -> Self {
        Self {
            source_content_id: value.source_content.id.to_hex(),
            source_bytes_hashed: value.source_content.bytes_hashed,
            source_sample_rate_hz: value.source_metadata.sample_rate_hz,
            source_channels: value.source_metadata.channels,
            source_frame_count: value.source_metadata.frame_count.0,
            decoder: value.decode.backend.clone(),
            decoder_version: value.decode.backend_version.clone(),
            selected_track_id: value.decode.selected_track_id,
            decode_verification: match value.decode.verification {
                DecodeIntegrity::Passed => "passed",
                DecodeIntegrity::Unavailable => "unavailable",
            }
            .into(),
            sample_rate_recipe: value.sample_rate.as_ref().map(|recipe| {
                AssetSampleRateRecipeRecord {
                    backend: recipe.backend.clone(),
                    backend_version: recipe.backend_version.clone(),
                    algorithm: recipe.algorithm.clone(),
                    input_sample_rate_hz: recipe.input_sample_rate_hz,
                    output_sample_rate_hz: recipe.output_sample_rate_hz,
                    channels: recipe.channels,
                    input_frames: recipe.input_frames,
                    output_frames: recipe.output_frames,
                    chunk_frames: recipe.chunk_frames as u64,
                    sinc_length: recipe.sinc_length as u64,
                    cutoff_bits: recipe.cutoff_bits,
                    oversampling_factor: recipe.oversampling_factor as u64,
                    interpolation: recipe.interpolation.clone(),
                    window: recipe.window.clone(),
                    delay_removed: recipe.delay_removed,
                    trimmed_output_frames: recipe.trimmed_output_frames,
                }
            }),
        }
    }

    fn validate(&self, asset: &AssetRecord) -> Result<(), ProjectIoError> {
        if self.source_content_id.is_empty()
            || self.source_bytes_hashed == 0
            || self.source_sample_rate_hz == 0
            || self.source_channels == 0
            || self.source_frame_count == 0
            || self.decoder.trim().is_empty()
            || self.decoder_version.trim().is_empty()
            || !matches!(self.decode_verification.as_str(), "passed" | "unavailable")
        {
            return Err(ProjectIoError::Invalid(format!(
                "asset {} has incomplete materialization provenance",
                asset.id
            )));
        }
        if let Some(recipe) = &self.sample_rate_recipe {
            if recipe.backend.trim().is_empty()
                || recipe.backend_version.trim().is_empty()
                || recipe.algorithm.trim().is_empty()
                || recipe.input_sample_rate_hz != self.source_sample_rate_hz
                || recipe.output_sample_rate_hz != asset.sample_rate_hz
                || recipe.channels != self.source_channels
                || recipe.channels != asset.channels
                || recipe.input_frames != self.source_frame_count
                || recipe.output_frames != asset.frame_count
                || recipe.chunk_frames == 0
                || recipe.sinc_length == 0
                || recipe.oversampling_factor == 0
                || recipe.interpolation.trim().is_empty()
                || recipe.window.trim().is_empty()
                || !recipe.delay_removed
            {
                return Err(ProjectIoError::Invalid(format!(
                    "asset {} has an invalid sample-rate recipe",
                    asset.id
                )));
            }
        }
        Ok(())
    }
}

impl AssetRecord {
    fn from_asset(asset: &MediaAsset) -> Self {
        let metadata = asset.metadata();
        let fingerprint = asset.content();
        Self {
            id: asset.id().0,
            name: asset.name().to_owned(),
            path: AssetPathIntent::from_location(asset.location()),
            content_algorithm: match fingerprint.algorithm {
                crate::assets::ContentHashAlgorithm::Fnv1a128NonCryptographic => {
                    "fnv1a-128/non-cryptographic".into()
                }
            },
            content_id: fingerprint.id.to_hex(),
            bytes_hashed: fingerprint.bytes_hashed,
            sample_rate_hz: metadata.sample_rate_hz,
            channels: metadata.channels,
            frame_count: metadata.frame_count.0,
            container: metadata.container.clone(),
            codec: metadata.codec.clone(),
            bit_depth: metadata.bit_depth,
            origin: asset_origin_label(asset.provenance().origin()),
            availability: asset_availability_label(asset.availability()),
            tags: asset.tags().clone(),
            favorite: asset.is_favorite(),
            materialization: asset
                .provenance()
                .materialization()
                .map(AssetMaterializationRecord::from_model),
        }
    }

    fn validate(&self) -> Result<(), ProjectIoError> {
        if self.id == 0 || self.name.trim().is_empty() || self.content_id.is_empty() {
            return Err(ProjectIoError::Invalid(
                "asset id, name, and fingerprint are required".into(),
            ));
        }
        if self.bytes_hashed == 0
            || self.sample_rate_hz == 0
            || self.channels == 0
            || self.frame_count == 0
        {
            return Err(ProjectIoError::Invalid(format!(
                "asset {} has invalid audio metadata",
                self.id
            )));
        }
        if self.bit_depth == Some(0) || self.content_algorithm.trim().is_empty() {
            return Err(ProjectIoError::Invalid(format!(
                "asset {} has invalid fingerprint metadata",
                self.id
            )));
        }
        self.path.validate()?;
        if let Some(materialization) = &self.materialization {
            materialization.validate(self)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainSectionRecord {
    pub domain: String,
    pub schema_version: u32,
    pub revision: u64,
    /// Relative key under the project package.  The actual domain codec owns
    /// this payload; keeping it external allows selective/lazy loading.
    pub payload_key: PathBuf,
    pub encoding: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingRecord {
    pub map: String,
    pub left: u64,
    pub right: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryMetadata {
    /// `false` is a normal manual save. `true` means this document was written
    /// to an autosave sidecar and should only be promoted with user consent.
    #[serde(default)]
    pub is_autosave: bool,
    #[serde(default)]
    pub saved_unix_ms: u64,
    #[serde(default)]
    pub base_project_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_file_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub format: String,
    pub version: u32,
    pub project_name: String,
    pub aggregate_revision: u64,
    #[serde(default)]
    pub sections: Vec<DomainSectionRecord>,
    #[serde(default)]
    pub bindings: Vec<BindingRecord>,
    #[serde(default)]
    pub assets: Vec<AssetRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceSnapshotDto>,
    #[serde(default)]
    pub recovery: RecoveryMetadata,
    /// Vendor/domain payload metadata which this build does not understand is
    /// retained verbatim.  Domain codecs may use these to point to blobs,
    /// model configurations, or plugin state.
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl ProjectFile {
    pub fn from_project(project: &DawProject, workspace: Option<WorkspaceSnapshotDto>) -> Self {
        let intent = project.save_intent();
        Self::from_intent(&intent, project, workspace)
    }

    pub fn from_intent(
        intent: &ProjectSaveIntent,
        project: &DawProject,
        workspace: Option<WorkspaceSnapshotDto>,
    ) -> Self {
        let mut sections = intent
            .sections
            .iter()
            .map(section_from_intent)
            .collect::<Vec<_>>();
        sections.sort_by(|a, b| a.domain.cmp(&b.domain));
        let mut bindings = intent
            .bindings
            .iter()
            .map(|row| BindingRecord {
                map: row.map.into(),
                left: row.left,
                right: row.right,
            })
            .collect::<Vec<_>>();
        bindings.sort_by(|a, b| (&a.map, a.left, a.right).cmp(&(&b.map, b.left, b.right)));
        let assets = project
            .state()
            .domains
            .assets
            .assets()
            .values()
            .map(AssetRecord::from_asset)
            .collect();
        Self {
            format: PROJECT_FILE_FORMAT.into(),
            version: PROJECT_FILE_VERSION,
            project_name: intent.name.clone(),
            aggregate_revision: intent.revision.aggregate,
            sections,
            bindings,
            assets,
            workspace,
            recovery: RecoveryMetadata::default(),
            extensions: BTreeMap::new(),
        }
    }

    /// Read the authoritative dynamic workspace document, if this package
    /// has one. The legacy `workspace` field remains a migration input only.
    pub fn workspace_document(&self) -> Result<Option<WorkspaceDocument>, ProjectIoError> {
        let Some(value) = self.extensions.get(WORKSPACE_DOCUMENT_EXTENSION_KEY) else {
            return Ok(None);
        };
        let document: WorkspaceDocument =
            serde_json::from_value(value.clone()).map_err(ProjectIoError::json)?;
        document.validate().map_err(|error| {
            ProjectIoError::Invalid(format!("invalid workspace document: {error}"))
        })?;
        Ok(Some(document))
    }

    /// Set or remove the authoritative workspace document without touching
    /// unrelated extension payloads. The document remains JSON data, never a
    /// Guise runtime snapshot.
    pub fn set_workspace_document(
        &mut self,
        document: Option<&WorkspaceDocument>,
    ) -> Result<(), ProjectIoError> {
        let Some(document) = document else {
            self.extensions.remove(WORKSPACE_DOCUMENT_EXTENSION_KEY);
            return Ok(());
        };
        document.validate().map_err(|error| {
            ProjectIoError::Invalid(format!("invalid workspace document: {error}"))
        })?;
        let value = serde_json::to_value(document).map_err(ProjectIoError::json)?;
        self.extensions
            .insert(WORKSPACE_DOCUMENT_EXTENSION_KEY.into(), value);
        Ok(())
    }

    pub fn content_roots(&self) -> Result<ProjectContentRoots, ProjectIoError> {
        let Some(value) = self.extensions.get(CONTENT_ROOTS_EXTENSION_KEY) else {
            return Ok(ProjectContentRoots::default());
        };
        let roots: ProjectContentRoots =
            serde_json::from_value(value.clone()).map_err(ProjectIoError::json)?;
        roots.validate()?;
        Ok(roots)
    }

    /// Replace the project-owned CAS roots while preserving unrelated future
    /// extensions. Empty roots remove the extension rather than serializing a
    /// meaningless cache marker.
    pub fn set_content_roots(
        &mut self,
        mut roots: ProjectContentRoots,
    ) -> Result<(), ProjectIoError> {
        roots.roots.sort_by(|left, right| {
            (&left.role, &left.logical_id).cmp(&(&right.role, &right.logical_id))
        });
        roots.validate()?;
        if roots.roots.is_empty() {
            self.extensions.remove(CONTENT_ROOTS_EXTENSION_KEY);
            return Ok(());
        }
        let value = serde_json::to_value(roots).map_err(ProjectIoError::json)?;
        self.extensions
            .insert(CONTENT_ROOTS_EXTENSION_KEY.into(), value);
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ProjectIoError> {
        if self.format != PROJECT_FILE_FORMAT {
            return Err(ProjectIoError::Invalid(format!(
                "unsupported project format {}",
                self.format
            )));
        }
        if self.version != PROJECT_FILE_VERSION {
            return Err(ProjectIoError::UnsupportedVersion {
                found: self.version,
                supported: PROJECT_FILE_VERSION,
            });
        }
        if self.project_name.trim().is_empty() {
            return Err(ProjectIoError::Invalid("project name is empty".into()));
        }
        let mut domains = BTreeSet::new();
        for section in &self.sections {
            if section.domain.trim().is_empty()
                || section.schema_version == 0
                || section.encoding.trim().is_empty()
                || !safe_relative_path(&section.payload_key)
            {
                return Err(ProjectIoError::Invalid(format!(
                    "invalid domain section {}",
                    section.domain
                )));
            }
            if !domains.insert(&section.domain) {
                return Err(ProjectIoError::Invalid(format!(
                    "duplicate domain section {}",
                    section.domain
                )));
            }
        }
        let mut asset_ids = BTreeSet::new();
        for asset in &self.assets {
            asset.validate()?;
            if !asset_ids.insert(asset.id) {
                return Err(ProjectIoError::Invalid(format!(
                    "duplicate asset id {}",
                    asset.id
                )));
            }
        }
        if let Some(workspace) = &self.workspace {
            workspace
                .validate()
                .map_err(|error| ProjectIoError::Invalid(format!("invalid workspace: {error}")))?;
        }
        self.workspace_document()?;
        self.content_roots()?;
        if self.recovery.is_autosave
            && self
                .recovery
                .primary_file_name
                .as_deref()
                .unwrap_or("")
                .is_empty()
        {
            return Err(ProjectIoError::Invalid(
                "autosave lacks its primary filename".into(),
            ));
        }
        Ok(())
    }

    pub fn encode_pretty(&self) -> Result<Vec<u8>, ProjectIoError> {
        self.validate()?;
        let mut bytes = serde_json::to_vec_pretty(self).map_err(ProjectIoError::json)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<(Self, Vec<ProjectIoDiagnostic>), ProjectIoError> {
        let value: Value = serde_json::from_slice(bytes).map_err(ProjectIoError::json)?;
        let (file, diagnostics) = migrate_value(value)?;
        file.validate()?;
        Ok((file, diagnostics))
    }
}

fn valid_content_root_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b'_' | b'/' | b':')
        })
        && !value.contains("..")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectIoDiagnostic {
    pub level: DiagnosticLevel,
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct LoadedProjectFile {
    pub file: ProjectFile,
    pub diagnostics: Vec<ProjectIoDiagnostic>,
    pub recovery: Option<RecoveryCandidate>,
}

#[derive(Clone, Debug)]
pub struct RecoveryCandidate {
    pub path: PathBuf,
    pub saved_unix_ms: u64,
    pub base_project_revision: u64,
}

pub fn load(path: &Path) -> Result<LoadedProjectFile, ProjectIoError> {
    let (file, mut diagnostics) =
        ProjectFile::decode(&fs::read(path).map_err(ProjectIoError::Io)?)?;
    let recovery_path = autosave_path(path)?;
    let recovery = if recovery_path.is_file() {
        match ProjectFile::decode(&fs::read(&recovery_path).map_err(ProjectIoError::Io)?) {
            Ok((autosave, _))
                if autosave.recovery.is_autosave
                    && autosave.recovery.base_project_revision > file.aggregate_revision =>
            {
                diagnostics.push(ProjectIoDiagnostic {
                    level: DiagnosticLevel::Warning,
                    code: "newer-autosave",
                    message: format!(
                        "a recoverable autosave exists at {}",
                        recovery_path.display()
                    ),
                });
                Some(RecoveryCandidate {
                    path: recovery_path,
                    saved_unix_ms: autosave.recovery.saved_unix_ms,
                    base_project_revision: autosave.recovery.base_project_revision,
                })
            }
            Ok(_) => None,
            Err(error) => {
                diagnostics.push(ProjectIoDiagnostic {
                    level: DiagnosticLevel::Warning,
                    code: "invalid-autosave",
                    message: format!("ignored invalid autosave: {error}"),
                });
                None
            }
        }
    } else {
        None
    };
    Ok(LoadedProjectFile {
        file,
        diagnostics,
        recovery,
    })
}

pub fn save_atomic(path: &Path, file: &ProjectFile) -> Result<(), ProjectIoError> {
    atomic_write(path, &file.encode_pretty()?)
}

pub fn save_autosave(
    path: &Path,
    file: &ProjectFile,
    saved_unix_ms: u64,
) -> Result<PathBuf, ProjectIoError> {
    let mut autosave = file.clone();
    autosave.recovery = RecoveryMetadata {
        is_autosave: true,
        saved_unix_ms,
        base_project_revision: file.aggregate_revision,
        primary_file_name: path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned),
    };
    let autosave_path = autosave_path(path)?;
    atomic_write(&autosave_path, &autosave.encode_pretty()?)?;
    Ok(autosave_path)
}

/// A successful explicit save makes an older autosave irrelevant. Failure to
/// delete it is deliberately non-fatal: a stale sidecar is safer than losing
/// a potentially newer recovery point.
pub fn clear_autosave(path: &Path) -> Result<bool, ProjectIoError> {
    let path = autosave_path(path)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ProjectIoError::Io(error)),
    }
}

pub fn autosave_path(path: &Path) -> Result<PathBuf, ProjectIoError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ProjectIoError::Invalid("project path must have a UTF-8 filename".into()))?;
    Ok(path.with_file_name(format!(".{name}.autosave")))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProjectIoError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(ProjectIoError::Invalid(format!(
            "project directory does not exist: {}",
            parent.display()
        )));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ProjectIoError::Invalid("project path must have a UTF-8 filename".into()))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{name}.write-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<(), ProjectIoError> {
        let mut temporary_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(ProjectIoError::Io)?;
        temporary_file
            .write_all(bytes)
            .map_err(ProjectIoError::Io)?;
        temporary_file.sync_all().map_err(ProjectIoError::Io)?;
        drop(temporary_file);
        fs::rename(&temporary, path).map_err(ProjectIoError::Io)?;
        // Best-effort directory fsync covers the rename on Unix. It is not
        // universally supported (notably on some Windows filesystems).
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn section_from_intent(section: &crate::daw_project::DomainSaveSection) -> DomainSectionRecord {
    DomainSectionRecord {
        domain: domain_name(section.domain).into(),
        schema_version: section.schema_version,
        revision: section.revision,
        payload_key: PathBuf::from(&section.payload_key),
        encoding: section.encoding.clone(),
    }
}

fn domain_name(domain: ProjectDomain) -> &'static str {
    match domain {
        ProjectDomain::Arrangement => "arrangement",
        ProjectDomain::Sequencer => "sequencer",
        ProjectDomain::Automation => "automation",
        ProjectDomain::Assets => "assets",
        ProjectDomain::SampleKits => "sample_kits",
        ProjectDomain::Mixer => "mixer",
        ProjectDomain::Air => "air",
        ProjectDomain::Bindings => "bindings",
    }
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn asset_origin_label(origin: &AssetOrigin) -> String {
    match origin {
        AssetOrigin::ImportedFile { importer } => format!("imported:{importer}"),
        AssetOrigin::RecordedInput { device } => format!("recorded:{device}"),
        AssetOrigin::Rendered {
            renderer,
            source_revision,
        } => format!("rendered:{renderer}@{source_revision}"),
        AssetOrigin::Generated { generator } => format!("generated:{generator}"),
        AssetOrigin::Migrated { source_format } => format!("migrated:{source_format}"),
    }
}

fn asset_availability_label(availability: &AssetAvailability) -> String {
    match availability {
        AssetAvailability::Present => "present".into(),
        AssetAvailability::Missing { checked_at_unix_ms } => {
            format!("missing:{checked_at_unix_ms}")
        }
        AssetAvailability::Relinked {
            relinked_at_unix_ms,
            ..
        } => format!("relinked:{relinked_at_unix_ms}"),
    }
}

fn migrate_value(value: Value) -> Result<(ProjectFile, Vec<ProjectIoDiagnostic>), ProjectIoError> {
    let version = match value.get("version") {
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .ok_or_else(|| ProjectIoError::Corrupt("project version is not a u32".into()))?,
        Some(_) => {
            return Err(ProjectIoError::Corrupt(
                "project version is not an integer".into(),
            ));
        }
        None => 0,
    };
    if version > PROJECT_FILE_VERSION {
        return Err(ProjectIoError::UnsupportedVersion {
            found: version,
            supported: PROJECT_FILE_VERSION,
        });
    }
    if version == PROJECT_FILE_VERSION {
        let file = serde_json::from_value(value).map_err(ProjectIoError::json)?;
        return Ok((file, Vec::new()));
    }
    // v0 was a private preview that used `name`, `revision`, and no explicit
    // format marker.  Migrate only those known fields; never guess domain data.
    let object = value
        .as_object()
        .ok_or_else(|| ProjectIoError::Corrupt("project root is not an object".into()))?;
    let project_name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ProjectIoError::Corrupt("v0 project has no name".into()))?;
    let aggregate_revision = object.get("revision").and_then(Value::as_u64).unwrap_or(0);
    Ok((ProjectFile { format: PROJECT_FILE_FORMAT.into(), version: PROJECT_FILE_VERSION, project_name: project_name.into(), aggregate_revision, sections: Vec::new(), bindings: Vec::new(), assets: Vec::new(), workspace: None, recovery: RecoveryMetadata::default(), extensions: BTreeMap::new() }, vec![ProjectIoDiagnostic { level: DiagnosticLevel::Warning, code: "migrated-v0", message: "migrated a legacy v0 project envelope; domain payloads must be re-saved by current codecs".into() }]))
}

#[derive(Debug)]
pub enum ProjectIoError {
    Io(io::Error),
    Json(String),
    Corrupt(String),
    Invalid(String),
    UnsupportedVersion { found: u32, supported: u32 },
}

impl ProjectIoError {
    fn json(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}
impl fmt::Display for ProjectIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "project I/O failed: {error}"),
            Self::Json(error) => write!(f, "project JSON is malformed: {error}"),
            Self::Corrupt(error) => write!(f, "project file is corrupt: {error}"),
            Self::Invalid(error) => write!(f, "project is invalid: {error}"),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "project version {found} is newer or unsupported (this audec supports {supported})"
            ),
        }
    }
}
impl Error for ProjectIoError {}
impl From<io::Error> for ProjectIoError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_document::WorkspaceDocument;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn example() -> ProjectFile {
        ProjectFile {
            format: PROJECT_FILE_FORMAT.into(),
            version: PROJECT_FILE_VERSION,
            project_name: "Like a Pen study".into(),
            aggregate_revision: 12,
            sections: vec![DomainSectionRecord {
                domain: "arrangement".into(),
                schema_version: 1,
                revision: 12,
                payload_key: PathBuf::from("arrangement.json"),
                encoding: "json".into(),
            }],
            bindings: vec![],
            assets: vec![AssetRecord {
                id: 1,
                name: "source".into(),
                path: AssetPathIntent {
                    project_relative: Some(PathBuf::from("media/source.flac")),
                    original_absolute: Some(PathBuf::from("/Volumes/Music/source.flac")),
                },
                content_algorithm: "fnv1a-128/non-cryptographic".into(),
                content_id: "0123".into(),
                bytes_hashed: 4,
                sample_rate_hz: 48_000,
                channels: 2,
                frame_count: 123,
                container: Some("flac".into()),
                codec: Some("flac".into()),
                bit_depth: Some(24),
                origin: "imported:drag-drop".into(),
                availability: "present".into(),
                tags: BTreeSet::new(),
                favorite: false,
                materialization: None,
            }],
            workspace: None,
            recovery: RecoveryMetadata::default(),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn round_trip_is_deterministic_and_portable() {
        let project = example();
        let first = project.encode_pretty().unwrap();
        let (decoded, diagnostics) = ProjectFile::decode(&first).unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(decoded, project);
        assert_eq!(decoded.encode_pretty().unwrap(), first);
        assert_eq!(
            decoded.assets[0]
                .path
                .candidates(Path::new("/work/show.audec"))[0],
            PathBuf::from("/work/media/source.flac")
        );
    }

    #[test]
    fn dynamic_workspace_document_round_trips_as_a_versioned_extension() {
        let mut project = example();
        let workspace = WorkspaceDocument::default();
        project.set_workspace_document(Some(&workspace)).unwrap();

        let bytes = project.encode_pretty().unwrap();
        let (loaded, diagnostics) = ProjectFile::decode(&bytes).unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(loaded.workspace_document().unwrap(), Some(workspace));
        assert_eq!(loaded.encode_pretty().unwrap(), bytes);
    }

    #[test]
    fn content_receipt_roots_survive_save_and_reopen() {
        let mut project = example();
        let receipt = ObjectRef::for_bytes(
            SchemaTag::reading_attachment("audec.render-product-receipt", 1).unwrap(),
            b"receipt",
        );
        project
            .set_content_roots(ProjectContentRoots {
                roots: vec![ContentRootRecord {
                    role: "render-product".into(),
                    logical_id: "master/0..1024".into(),
                    receipt: ContentObjectRecord::from_object(&receipt),
                }],
                ..ProjectContentRoots::default()
            })
            .unwrap();
        let bytes = project.encode_pretty().unwrap();
        let (loaded, diagnostics) = ProjectFile::decode(&bytes).unwrap();
        assert!(diagnostics.is_empty());
        let roots = loaded.content_roots().unwrap();
        assert_eq!(roots.roots[0].receipt.object_ref().unwrap(), receipt);
        assert_eq!(loaded.encode_pretty().unwrap(), bytes);
    }

    #[test]
    fn content_roots_refuse_weak_or_malformed_identity_records() {
        let mut project = example();
        project.extensions.insert(
            CONTENT_ROOTS_EXTENSION_KEY.into(),
            serde_json::json!({
                "format": CONTENT_ROOTS_FORMAT,
                "version": CONTENT_ROOTS_VERSION,
                "roots": [{
                    "role": "analysis-artifact",
                    "logical_id": "onsets",
                    "receipt": {
                        "schema_class": "reading-attachment",
                        "schema_name": "audec.analysis-artifact-receipt",
                        "schema_version": 1,
                        "algorithm": "fnv1a-128",
                        "sha256": "00",
                        "byte_len": 1
                    }
                }]
            }),
        );
        assert!(project.validate().is_err());
    }

    #[test]
    fn rejects_asset_and_payload_root_escape() {
        let mut project = example();
        project.assets[0].path.project_relative = Some(PathBuf::from("../steal.flac"));
        assert!(project.validate().is_err());
        project = example();
        project.sections[0].payload_key = PathBuf::from("../arrangement.json");
        assert!(project.validate().is_err());
    }

    #[test]
    fn atomic_save_and_autosave_surface_recovery() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("audec-project-io-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("study.audec");
        let project = example();
        save_atomic(&path, &project).unwrap();
        let mut changed = project.clone();
        changed.aggregate_revision = 13;
        save_autosave(&path, &changed, 100).unwrap();
        let loaded = load(&path).unwrap();
        assert!(loaded.recovery.is_some());
        assert!(loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "newer-autosave"));
        assert!(clear_autosave(&path).unwrap());
        assert!(load(&path).unwrap().recovery.is_none());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn migrates_explicit_v0_without_guessing_payloads() {
        let (project, diagnostics) =
            ProjectFile::decode(br#"{"name":"old experiment","revision":9}"#).unwrap();
        assert_eq!(project.version, PROJECT_FILE_VERSION);
        assert_eq!(project.project_name, "old experiment");
        assert_eq!(project.sections.len(), 0);
        assert_eq!(diagnostics[0].code, "migrated-v0");
    }
}
