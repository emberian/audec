//! High-level durable project service: codecs, package storage, media, and export.
//!
//! This module is intentionally independent of GPUI and `LiveProject`.  It
//! turns a validated [`DawProject`] snapshot into one package checkpoint, and
//! turns a package checkpoint back into a validated aggregate.  UI code owns
//! dialogs and entity updates; `LiveProject` owns editable locks; this service
//! owns neither.  Its only authority is durable bytes and immutable snapshots.
//!
//! AIR payload encoding is deliberately injected. The constructive codec owns
//! arrangement/sequencer/automation/assets/mixer/bindings today, but it
//! explicitly leaves AIR to a dedicated codec. A repository must therefore
//! fail visibly rather than saving a project after silently dropping claims.

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::assets::{AssetId, AssetOrigin};
use crate::daw_engine::AssetPcmMap;
use crate::daw_project::{DawProject, ProjectRevisions, DAW_PROJECT_SCHEMA_VERSION};
use crate::export::{
    export_revision_pinned_audio_to_wav, ExportError, ExportObserver, RevisionPinnedAudio,
    RevisionPinnedWavExportReport, WavExportRequest,
};
use crate::media_resolver::{
    resolve_material, MaterialRequest, MaterialResolution, MediaDecoder, RelinkProposal,
    ResolutionDiagnostic, RubatoSampleRateConverter, SampleRateConverter,
};
use crate::ontology::AuditoryIr;
use crate::project_codecs::{self, CodecError};
use crate::project_format::{PreservedProjectData, ProjectCheckpoint, ProjectFormatError};
use crate::project_io::{
    DiagnosticLevel, DomainSectionRecord, ProjectFile, ProjectIoDiagnostic, ProjectIoError,
    WORKSPACE_DOCUMENT_EXTENSION_KEY,
};
use crate::project_store::{
    JournalCompactionResult, LoadedCheckpoint, ProjectStore, ProjectStoreError, RecoveryCheckpoint,
    RecoveryDiscovery, SaveResult,
};
use crate::sample_material::{canonical_pcm_identity, encode_canonical_pcm, DecodedPcmView};
use crate::workspace_document::WorkspaceDocument;

const CONSTRUCTIVE_DOMAINS: [&str; 8] = [
    "arrangement",
    "sequencer",
    "automation",
    "assets",
    "mixer",
    "sample_kits",
    "air",
    "bindings",
];

/// Codec boundary for AIR's independently versioned claim graph.  A later AIR
/// codec can use the exact `air` section descriptor/payload without changing
/// any package, autosave, or UI-facing repository operation.
pub trait AirPayloadCodec {
    fn encode_air(&self, air: &AuditoryIr) -> Result<Vec<u8>, AirPayloadError>;

    fn decode_air(
        &self,
        descriptor: &DomainSectionRecord,
        bytes: &[u8],
    ) -> Result<AuditoryIr, AirPayloadError>;
}

/// A bridge for the already-supported empty-AIR case. It is useful for
/// ordinary authored projects, while rejecting nonempty AIR rather than
/// pretending current constructive codecs can serialize claims they cannot.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyAirPayloadCodec;

/// Structural equality for the lossless-decode check.
///
/// `canonical` is the typed graph re-serialized; `original` is the payload as
/// written. Every key and element must survive, but a number written from an
/// `f32` field parses as an `f64` that the typed round trip cannot reproduce
/// bit for bit, so two numbers are the same when they are equal or when they
/// agree after narrowing to `f32`. Nothing else is tolerated.
fn json_preserved(canonical: &serde_json::Value, original: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (canonical, original) {
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| json_preserved(value, other))
                })
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(value, other)| json_preserved(value, other))
        }
        (Value::Number(left), Value::Number(right)) => {
            left == right
                || match (left.as_f64(), right.as_f64()) {
                    (Some(left), Some(right)) => {
                        left.is_finite() && right.is_finite() && left as f32 == right as f32
                    }
                    _ => false,
                }
        }
        _ => canonical == original,
    }
}

/// Production AIR codec for the `air` payload already declared by the project
/// envelope. The payload is the complete [`AuditoryIr`] JSON object; no graph
/// member is projected into a second, partial representation.
///
/// Decode is deliberately stricter than Serde's default behavior. Duplicate
/// object keys and fields unknown to this build are refused because accepting
/// either would make a subsequent save lossy. Older packages written by
/// [`EmptyAirPayloadCodec`] remain readable when their explicit `empty` flag
/// is true.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonAirPayloadCodec;

impl AirPayloadCodec for JsonAirPayloadCodec {
    fn encode_air(&self, air: &AuditoryIr) -> Result<Vec<u8>, AirPayloadError> {
        if air.schema_version != AuditoryIr::CURRENT_SCHEMA_VERSION {
            return Err(AirPayloadError::UnsupportedSchema(air.schema_version));
        }
        validate_air_graph(air).map_err(AirPayloadError::Encoding)?;
        let mut bytes = serde_json::to_vec_pretty(air)
            .map_err(|error| AirPayloadError::Encoding(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn decode_air(
        &self,
        descriptor: &DomainSectionRecord,
        bytes: &[u8],
    ) -> Result<AuditoryIr, AirPayloadError> {
        validate_air_descriptor(descriptor)?;
        let value = serde_json::from_slice::<UniqueJsonValue>(bytes)
            .map_err(|error| AirPayloadError::Decoding(error.to_string()))?
            .0;
        let object = value.as_object().ok_or_else(|| {
            AirPayloadError::Decoding("AIR payload root must be a JSON object".into())
        })?;
        let payload_schema = object
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u32::try_from(version).ok())
            .ok_or_else(|| {
                AirPayloadError::Decoding(
                    "AIR payload schema_version must be an unsigned 32-bit integer".into(),
                )
            })?;
        if payload_schema != AuditoryIr::CURRENT_SCHEMA_VERSION {
            return Err(AirPayloadError::UnsupportedSchema(payload_schema));
        }

        if object.contains_key("empty") {
            return decode_legacy_empty_air(value);
        }

        let air: AuditoryIr = serde_json::from_value(value.clone())
            .map_err(|error| AirPayloadError::Decoding(error.to_string()))?;
        let canonical = serde_json::to_value(&air)
            .map_err(|error| AirPayloadError::Decoding(error.to_string()))?;
        if !json_preserved(&canonical, &value) {
            return Err(AirPayloadError::Decoding(
                "AIR payload contains fields or values this schema cannot preserve losslessly"
                    .into(),
            ));
        }
        validate_air_graph(&air).map_err(AirPayloadError::Decoding)?;
        Ok(air)
    }
}

fn validate_air_descriptor(descriptor: &DomainSectionRecord) -> Result<(), AirPayloadError> {
    if descriptor.domain != "air" {
        return Err(AirPayloadError::Decoding(format!(
            "AIR codec was given the {} section",
            descriptor.domain
        )));
    }
    if descriptor.encoding != project_codecs::JSON_ENCODING {
        return Err(AirPayloadError::Decoding(format!(
            "AIR payload encoding {} is unsupported",
            descriptor.encoding
        )));
    }
    if descriptor.schema_version != AuditoryIr::CURRENT_SCHEMA_VERSION {
        return Err(AirPayloadError::UnsupportedSchema(
            descriptor.schema_version,
        ));
    }
    Ok(())
}

fn validate_air_graph(air: &AuditoryIr) -> Result<(), String> {
    let issues = air.validate();
    if !issues.is_empty() {
        return Err(format!(
            "AIR graph is invalid: {}",
            issues
                .iter()
                .map(|issue| format!("{}: {}", issue.path, issue.message))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(())
}

fn decode_legacy_empty_air(value: serde_json::Value) -> Result<AuditoryIr, AirPayloadError> {
    let object = value.as_object().expect("caller checked AIR object");
    let expected = ["empty", "sample_rate", "schema_version"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(AirPayloadError::Decoding(
            "legacy empty AIR payload contains fields this codec cannot preserve".into(),
        ));
    }
    let decoded: EmptyAirDto = serde_json::from_value(value)
        .map_err(|error| AirPayloadError::Decoding(error.to_string()))?;
    if !decoded.empty {
        return Err(AirPayloadError::Decoding(
            "legacy AIR payload declares nonempty data without encoding it".into(),
        ));
    }
    if decoded.sample_rate == 0 {
        return Err(AirPayloadError::Decoding(
            "AIR payload has a zero sample rate".into(),
        ));
    }
    Ok(AuditoryIr::new(decoded.sample_rate))
}

/// A JSON value whose object visitor rejects duplicate keys at every depth.
/// `serde_json::Value` alone keeps the final duplicate and would make that
/// ambiguous source payload appear safe to rewrite.
struct UniqueJsonValue(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .map(UniqueJsonValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(serde_json::Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(serde_json::Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        UniqueJsonValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueJsonValue>()? {
            values.push(value.0);
        }
        Ok(UniqueJsonValue(serde_json::Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object key {key:?}"
                )));
            }
            let value = map.next_value::<UniqueJsonValue>()?;
            values.insert(key, value.0);
        }
        Ok(UniqueJsonValue(serde_json::Value::Object(values)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EmptyAirDto {
    schema_version: u32,
    sample_rate: u32,
    empty: bool,
}

impl AirPayloadCodec for EmptyAirPayloadCodec {
    fn encode_air(&self, air: &AuditoryIr) -> Result<Vec<u8>, AirPayloadError> {
        if !air_is_empty(air) {
            return Err(AirPayloadError::NonEmptyAirRequiresCodec);
        }
        let mut bytes = serde_json::to_vec_pretty(&EmptyAirDto {
            schema_version: air.schema_version,
            sample_rate: air.sample_rate,
            empty: true,
        })
        .map_err(|error| AirPayloadError::Encoding(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn decode_air(
        &self,
        descriptor: &DomainSectionRecord,
        bytes: &[u8],
    ) -> Result<AuditoryIr, AirPayloadError> {
        if descriptor.schema_version != AuditoryIr::CURRENT_SCHEMA_VERSION {
            return Err(AirPayloadError::UnsupportedSchema(
                descriptor.schema_version,
            ));
        }
        let decoded: EmptyAirDto = serde_json::from_slice(bytes)
            .map_err(|error| AirPayloadError::Decoding(error.to_string()))?;
        if !decoded.empty {
            return Err(AirPayloadError::Decoding(
                "empty AIR codec was given a nonempty payload".into(),
            ));
        }
        if decoded.schema_version != AuditoryIr::CURRENT_SCHEMA_VERSION {
            return Err(AirPayloadError::UnsupportedSchema(decoded.schema_version));
        }
        if decoded.sample_rate == 0 {
            return Err(AirPayloadError::Decoding(
                "AIR payload has a zero sample rate".into(),
            ));
        }
        Ok(AuditoryIr::new(decoded.sample_rate))
    }
}

fn air_is_empty(air: &AuditoryIr) -> bool {
    air.sources.is_empty()
        && air.spans.is_empty()
        && air.objects.is_empty()
        && air.transforms.is_empty()
        && air.parameters.is_empty()
        && air.automations.is_empty()
        && air.modulations.is_empty()
        && air.relations.is_empty()
        && air.evidence.is_empty()
        && air.hypotheses.is_empty()
        && air.hypothesis_sets.is_empty()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AirPayloadError {
    NonEmptyAirRequiresCodec,
    UnsupportedSchema(u32),
    Encoding(String),
    Decoding(String),
}

impl fmt::Display for AirPayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonEmptyAirRequiresCodec => formatter.write_str(
                "this build has no AIR claim-graph codec; refusing to drop nonempty AIR",
            ),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported AIR payload schema {version}")
            }
            Self::Encoding(message) => write!(formatter, "could not encode AIR: {message}"),
            Self::Decoding(message) => write!(formatter, "could not decode AIR: {message}"),
        }
    }
}

impl Error for AirPayloadError {}

/// A fully decoded aggregate plus data this build retained but does not edit.
#[derive(Clone, Debug)]
pub struct OpenedProject {
    pub project: DawProject,
    /// The portable, dynamic workspace document. A v1 six-view snapshot is
    /// migrated into this form on open and should be passed back on save.
    pub workspace: Option<WorkspaceDocument>,
    pub preserved: PreservedProjectData,
    pub diagnostics: Vec<ProjectIoDiagnostic>,
    pub manifest_path: PathBuf,
}

/// Media hydration results remain separate from the decoded project. A missing
/// route never prevents a project from opening, and a relink is always an
/// explicit follow-up command rather than a resolver side effect.
#[derive(Clone, Debug, Default)]
pub struct MediaHydration {
    pub pcm: AssetPcmMap,
    pub resolved_assets: Vec<AssetId>,
    pub unresolved_assets: Vec<AssetId>,
    pub relink_proposals: Vec<RelinkProposal>,
    pub diagnostics: Vec<MediaHydrationDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaHydrationDiagnostic {
    pub asset: AssetId,
    pub path: Option<PathBuf>,
    pub code: &'static str,
    pub message: String,
}

/// A policy recommendation, never an automatic restore.  Recovery is offered
/// only when its durable snapshot is newer than the primary checkpoint; the
/// app must still ask the person which interpretation of the interrupted
/// session to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryPreference {
    PrimaryIsCurrent {
        primary_revision: Option<u64>,
    },
    OfferRecovery {
        primary_revision: Option<u64>,
        recovery: RecoveryCheckpoint,
    },
}

/// Project-level persistence service. It is safe to use from a worker thread
/// because all inputs and outputs are owned snapshots; callers decide how to
/// marshal results back to a UI or live-project controller.
#[derive(Clone, Debug)]
pub struct ProjectRepository<C> {
    store: ProjectStore,
    air_codec: C,
}

impl<C> ProjectRepository<C>
where
    C: AirPayloadCodec,
{
    pub fn new(store: ProjectStore, air_codec: C) -> Self {
        Self { store, air_codec }
    }

    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    pub fn recovery_discovery(&self) -> RecoveryDiscovery {
        self.store.discover_recovery()
    }

    /// Determine whether a discovered recovery checkpoint is newer than the
    /// primary package. This exposes a labeled choice instead of silently
    /// promoting an autosave over an explicit save.
    pub fn recovery_preference(&self) -> Result<RecoveryPreference, ProjectRepositoryError> {
        let discovery = self.recovery_discovery();
        let primary_revision = if discovery.primary.is_some() {
            Some(
                self.store
                    .load_primary()
                    .map_err(ProjectRepositoryError::Store)?
                    .checkpoint
                    .file
                    .aggregate_revision,
            )
        } else {
            None
        };
        let recovery = discovery.checkpoints.into_iter().find(|candidate| {
            primary_revision
                .map(|revision| candidate.base_project_revision > revision)
                .unwrap_or(true)
        });
        Ok(match recovery {
            Some(recovery) => RecoveryPreference::OfferRecovery {
                primary_revision,
                recovery,
            },
            None => RecoveryPreference::PrimaryIsCurrent { primary_revision },
        })
    }

    /// Append one already-framed command-journal segment to the package. The
    /// command-envelope lane owns its encoding and checksums; this repository
    /// only provides durable, atomic placement beside recovery checkpoints.
    pub fn write_journal_segment(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, ProjectRepositoryError> {
        self.store
            .write_journal_segment(name, bytes)
            .map_err(ProjectRepositoryError::Store)
    }

    pub fn compact_journal_segments(
        &self,
        max_active_segments: usize,
    ) -> Result<JournalCompactionResult, ProjectRepositoryError> {
        self.store
            .compact_journal_segments(max_active_segments)
            .map_err(ProjectRepositoryError::Store)
    }

    pub fn rotate_recovery_checkpoints(
        &self,
        max_checkpoints: usize,
    ) -> Result<Vec<PathBuf>, ProjectRepositoryError> {
        self.store
            .rotate_recovery_checkpoints(max_checkpoints)
            .map_err(ProjectRepositoryError::Store)
    }

    /// Build and atomically publish a primary checkpoint. `preserved` must be
    /// carried from a prior open when saving an older build's project, so
    /// unknown payloads and envelope fields survive untouched.
    pub fn save_primary(
        &self,
        project: &DawProject,
        preserved: PreservedProjectData,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        let revision = project.revisions().aggregate;
        let checkpoint = self.build_checkpoint(project, preserved, None)?;
        self.store
            .save_primary(&checkpoint, revision)
            .map_err(ProjectRepositoryError::Store)
    }

    /// Save a project snapshot together with the full dynamic workspace
    /// document. This is the normal app-controller path; [`save_primary`] is
    /// retained for headless callers that intentionally have no workspace.
    pub fn save_primary_with_workspace(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        let revision = project.revisions().aggregate;
        let checkpoint = self.build_checkpoint(project, preserved, workspace)?;
        self.store
            .save_primary(&checkpoint, revision)
            .map_err(ProjectRepositoryError::Store)
    }

    /// Save a complete app snapshot, first making every Audec-generated PCM
    /// asset durable at its project-relative route. Media publication follows
    /// the same immutable-before-manifest law as domain payloads: a crash may
    /// leave an unreferenced file, but never a manifest pointing at half a
    /// template.
    pub fn save_primary_with_workspace_and_media(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
        pcm: &AssetPcmMap,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.persist_generated_media(project, pcm)?;
        self.save_primary_with_workspace(project, workspace, preserved)
    }

    /// Build and publish an explicit recovery checkpoint. Its revision guard
    /// lets a live controller avoid marking a project saved after newer edits.
    pub fn save_autosave(
        &self,
        project: &DawProject,
        preserved: PreservedProjectData,
        saved_unix_ms: u64,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        let revision = project.revisions().aggregate;
        let checkpoint = self.build_checkpoint(project, preserved, None)?;
        self.store
            .save_autosave(&checkpoint, revision, saved_unix_ms)
            .map_err(ProjectRepositoryError::Store)
    }

    /// Autosave the same complete document package as a primary save, but
    /// under recovery provenance. It never changes the primary manifest.
    pub fn save_autosave_with_workspace(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
        saved_unix_ms: u64,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        let revision = project.revisions().aggregate;
        let checkpoint = self.build_checkpoint(project, preserved, workspace)?;
        self.store
            .save_autosave(&checkpoint, revision, saved_unix_ms)
            .map_err(ProjectRepositoryError::Store)
    }

    pub fn save_autosave_with_workspace_and_media(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
        saved_unix_ms: u64,
        pcm: &AssetPcmMap,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.persist_generated_media(project, pcm)?;
        self.save_autosave_with_workspace(project, workspace, preserved, saved_unix_ms)
    }

    pub fn open_primary(&self) -> Result<OpenedProject, ProjectRepositoryError> {
        self.decode_loaded(
            self.store
                .load_primary()
                .map_err(ProjectRepositoryError::Store)?,
        )
    }

    pub fn open_recovery(
        &self,
        candidate: &RecoveryCheckpoint,
    ) -> Result<OpenedProject, ProjectRepositoryError> {
        self.decode_loaded(
            self.store
                .load_recovery(candidate)
                .map_err(ProjectRepositoryError::Store)?,
        )
    }

    /// Decode all registered media into an immutable PCM map. It returns a
    /// complete report even if some assets are absent or candidates fail their
    /// recorded identity checks.
    pub fn hydrate_media(
        &self,
        project: &DawProject,
        decoder: &impl MediaDecoder,
    ) -> MediaHydration {
        let mut hydration = MediaHydration::default();
        // Recovery manifests live below `recovery/`, but relative media paths
        // are always rooted at the package's canonical `project.json`.
        let package_manifest = self.store.package().manifest_path();
        for asset in project.state().domains.assets.assets().values() {
            let request = MaterialRequest::source(
                asset.id(),
                asset.name(),
                crate::project_io::AssetPathIntent::from_location(asset.location()),
                asset.source_metadata().clone(),
                asset.source_content(),
            );
            match resolve_material(decoder, &package_manifest, request) {
                MaterialResolution::Resolved(resolved) => {
                    let id = resolved.request.asset;
                    hydration.resolved_assets.push(id);
                    if let Some(proposal) = resolved.relink {
                        hydration.relink_proposals.push(proposal);
                    }
                    for diagnostic in resolved.diagnostics {
                        hydration.diagnostics.push(media_diagnostic(id, diagnostic));
                    }
                    match hydrate_resolved_pcm(asset, resolved.decoded.pcm) {
                        Ok(pcm) => {
                            hydration.pcm.insert(id, pcm);
                        }
                        Err(message) => {
                            hydration.resolved_assets.retain(|asset| *asset != id);
                            hydration.unresolved_assets.push(id);
                            hydration.diagnostics.push(MediaHydrationDiagnostic {
                                asset: id,
                                path: Some(resolved.decoded.path),
                                code: "materialization-failed",
                                message,
                            });
                        }
                    }
                }
                MaterialResolution::Unresolved(unresolved) => {
                    let id = unresolved.request.asset;
                    hydration.unresolved_assets.push(id);
                    hydration
                        .relink_proposals
                        .extend(unresolved.repair_candidates);
                    for diagnostic in unresolved.diagnostics {
                        hydration.diagnostics.push(media_diagnostic(id, diagnostic));
                    }
                }
            }
        }
        hydration.resolved_assets.sort();
        hydration.unresolved_assets.sort();
        hydration.relink_proposals.sort_by(|left, right| {
            left.asset
                .cmp(&right.asset)
                .then_with(|| left.new_path.cmp(&right.new_path))
        });
        hydration
    }

    fn persist_generated_media(
        &self,
        project: &DawProject,
        pcm: &AssetPcmMap,
    ) -> Result<(), ProjectRepositoryError> {
        for asset in project.state().domains.assets.assets().values() {
            let is_canonical_generated_pcm =
                matches!(asset.provenance().origin(), AssetOrigin::Generated { .. })
                    && asset.metadata().container.as_deref() == Some("audec-pcm")
                    && asset.metadata().codec.as_deref() == Some("f32le")
                    && asset.metadata().bit_depth == Some(32);
            if !is_canonical_generated_pcm {
                continue;
            }
            let relative = asset.location().project_relative.as_ref().ok_or(
                ProjectRepositoryError::MissingGeneratedMediaRoute(asset.id()),
            )?;
            let material = pcm
                .get(&asset.id())
                .ok_or(ProjectRepositoryError::MissingGeneratedPcm(asset.id()))?;
            let identity = canonical_pcm_identity(DecodedPcmView::from_pcm_asset(material))
                .map_err(|error| ProjectRepositoryError::GeneratedPcm {
                    asset: asset.id(),
                    message: error.to_string(),
                })?;
            if identity.fingerprint != asset.content()
                || identity.format.sample_rate.get() != asset.metadata().sample_rate_hz
                || identity.format.channels.get() != asset.metadata().channels
                || identity.frame_count != asset.metadata().frame_count.0
            {
                return Err(ProjectRepositoryError::GeneratedPcm {
                    asset: asset.id(),
                    message: "runtime PCM does not match the durable generated asset".into(),
                });
            }
            let bytes = encode_canonical_pcm(DecodedPcmView::from_pcm_asset(material)).map_err(
                |error| ProjectRepositoryError::GeneratedPcm {
                    asset: asset.id(),
                    message: error.to_string(),
                },
            )?;
            if crate::assets::ContentFingerprint::from_bytes(&bytes) != asset.content() {
                return Err(ProjectRepositoryError::GeneratedPcm {
                    asset: asset.id(),
                    message: "encoded media identity differs from the registered content".into(),
                });
            }
            self.store
                .publish_generated_media(relative, &bytes)
                .map_err(ProjectRepositoryError::Store)?;
        }
        Ok(())
    }

    /// Offline export is pinned to the revision that produced `audio`. This
    /// method never reads the mutable project, so a project can continue to be
    /// edited while a historical but honest export completes.
    pub fn export_revision_pinned<O: ExportObserver>(
        &self,
        pinned: RevisionPinnedAudio,
        request: &WavExportRequest,
        observer: &mut O,
    ) -> Result<RevisionPinnedWavExportReport, ProjectRepositoryError> {
        export_revision_pinned_audio_to_wav(pinned, request, observer)
            .map_err(ProjectRepositoryError::Export)
    }

    fn build_checkpoint(
        &self,
        project: &DawProject,
        mut preserved: PreservedProjectData,
        workspace: Option<&WorkspaceDocument>,
    ) -> Result<ProjectCheckpoint, ProjectRepositoryError> {
        let mut file = ProjectFile::from_project(project, None);
        // The workspace document is an envelope extension for compatibility,
        // but it is owned by this repository—not a foreign payload. Remove a
        // carried copy before combining preserved data so an edited workspace
        // cannot collide with its former value on save.
        let carried_workspace = preserved
            .envelope_extensions
            .remove(WORKSPACE_DOCUMENT_EXTENSION_KEY);
        match workspace {
            Some(workspace) => file
                .set_workspace_document(Some(workspace))
                .map_err(ProjectRepositoryError::Envelope)?,
            None => {
                if let Some(value) = carried_workspace {
                    file.extensions
                        .insert(WORKSPACE_DOCUMENT_EXTENSION_KEY.into(), value);
                    file.workspace_document()
                        .map_err(ProjectRepositoryError::Envelope)?;
                }
            }
        }
        let mut payloads =
            project_codecs::encode_constructive(project).map_err(ProjectRepositoryError::Codec)?;
        let air = project.state().domains.air.clone();
        let air_section = section(&file, "air")?;
        if payloads
            .0
            .insert(
                air_section.payload_key.clone(),
                self.air_codec
                    .encode_air(&air)
                    .map_err(ProjectRepositoryError::AirCodec)?,
            )
            .is_some()
        {
            return Err(ProjectRepositoryError::UnexpectedAirPayloadCollision(
                air_section.payload_key.clone(),
            ));
        }
        ProjectCheckpoint::new(file, payloads, preserved).map_err(ProjectRepositoryError::Format)
    }

    fn decode_loaded(
        &self,
        loaded: LoadedCheckpoint,
    ) -> Result<OpenedProject, ProjectRepositoryError> {
        let checkpoint = loaded.checkpoint;
        let workspace = workspace_from_file(&checkpoint.file)?;
        let recognized = recognized_domains();
        let preserved = PreservedProjectData::from_unrecognized(
            &checkpoint.file,
            &checkpoint.payloads,
            &recognized,
        )
        .map_err(ProjectRepositoryError::Format)?;
        let air_section = section(&checkpoint.file, "air")?;
        let air_bytes = checkpoint
            .payloads
            .get(&air_section.payload_key)
            .ok_or_else(|| {
                ProjectRepositoryError::MissingPayload(air_section.payload_key.clone())
            })?;
        let air = self
            .air_codec
            .decode_air(air_section, air_bytes)
            .map_err(ProjectRepositoryError::AirCodec)?;
        let decoded =
            project_codecs::decode_constructive(&checkpoint.file, &checkpoint.payloads, air)
                .map_err(ProjectRepositoryError::Codec)?;
        let revisions = revisions_from_file(&checkpoint.file)?;
        let project = DawProject::from_restored(
            decoded.name,
            DAW_PROJECT_SCHEMA_VERSION,
            decoded.state,
            revisions,
            checkpoint.file.aggregate_revision,
        )
        .map_err(ProjectRepositoryError::Bridge)?;
        let mut diagnostics = loaded.diagnostics;
        diagnostics.extend(decoded.diagnostics);
        if checkpoint
            .file
            .workspace_document()
            .map_err(ProjectRepositoryError::Envelope)?
            .is_none()
            && workspace.is_some()
        {
            diagnostics.push(ProjectIoDiagnostic {
                level: DiagnosticLevel::Info,
                code: "migrated-legacy-workspace",
                message: "migrated legacy fixed workspace snapshot to portable dynamic document"
                    .into(),
            });
        }
        Ok(OpenedProject {
            project,
            workspace,
            preserved,
            diagnostics,
            manifest_path: loaded.manifest_path,
        })
    }
}

fn hydrate_resolved_pcm(
    asset: &crate::assets::MediaAsset,
    source_pcm: crate::daw_render::PcmAsset,
) -> Result<crate::daw_render::PcmAsset, String> {
    let Some(materialization) = asset.provenance().materialization() else {
        return Ok(source_pcm);
    };
    let pcm = match &materialization.sample_rate {
        None => source_pcm,
        Some(recipe) => {
            let converter = RubatoSampleRateConverter::from_materialization_recipe(recipe)
                .map_err(|error| error.to_string())?;
            let converted = converter
                .convert(&source_pcm, recipe.output_sample_rate_hz)
                .map_err(|error| error.to_string())?;
            if converted.provenance.to_durable() != *recipe {
                return Err(
                    "current converter did not reproduce the persisted sample-rate recipe".into(),
                );
            }
            converted.pcm
        }
    };
    let metadata = asset.metadata();
    if pcm.format.sample_rate.get() != metadata.sample_rate_hz
        || pcm.format.channels.get() != metadata.channels
        || pcm.frame_count() != metadata.frame_count.0
    {
        return Err(format!(
            "materialized PCM is {} Hz/{} channels/{} frames, expected {} Hz/{} channels/{} frames",
            pcm.format.sample_rate.get(),
            pcm.format.channels.get(),
            pcm.frame_count(),
            metadata.sample_rate_hz,
            metadata.channels,
            metadata.frame_count.0
        ));
    }
    let identity = canonical_pcm_identity(DecodedPcmView::from_pcm_asset(&pcm))
        .map_err(|error| error.to_string())?;
    if identity.fingerprint != asset.content() {
        return Err(format!(
            "materialized PCM fingerprint {} differs from persisted {}",
            identity.fingerprint.id.to_hex(),
            asset.content().id.to_hex()
        ));
    }
    Ok(pcm)
}

fn workspace_from_file(
    file: &ProjectFile,
) -> Result<Option<WorkspaceDocument>, ProjectRepositoryError> {
    if let Some(document) = file
        .workspace_document()
        .map_err(ProjectRepositoryError::Envelope)?
    {
        return Ok(Some(document));
    }
    file.workspace
        .clone()
        .map(crate::workspace::migrate_legacy_snapshot)
        .transpose()
        .map_err(|error| ProjectRepositoryError::Workspace(error.to_string()))
}

fn media_diagnostic(asset: AssetId, diagnostic: ResolutionDiagnostic) -> MediaHydrationDiagnostic {
    MediaHydrationDiagnostic {
        asset,
        path: diagnostic.path,
        code: diagnostic.code,
        message: diagnostic.message,
    }
}

fn recognized_domains() -> BTreeSet<String> {
    CONSTRUCTIVE_DOMAINS
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn section<'a>(
    file: &'a ProjectFile,
    domain: &str,
) -> Result<&'a DomainSectionRecord, ProjectRepositoryError> {
    file.sections
        .iter()
        .find(|section| section.domain == domain)
        .ok_or_else(|| ProjectRepositoryError::MissingSection(domain.into()))
}

fn revisions_from_file(file: &ProjectFile) -> Result<ProjectRevisions, ProjectRepositoryError> {
    let revision = |domain: &str| -> Result<u64, ProjectRepositoryError> {
        Ok(section(file, domain)?.revision)
    };
    Ok(ProjectRevisions {
        aggregate: file.aggregate_revision,
        arrangement: revision("arrangement")?,
        sequencer: revision("sequencer")?,
        automation: revision("automation")?,
        assets: revision("assets")?,
        mixer: revision("mixer")?,
        sample_kits: revision("sample_kits")?,
        air: revision("air")?,
        bindings: revision("bindings")?,
    })
}

#[derive(Debug)]
pub enum ProjectRepositoryError {
    Store(ProjectStoreError),
    Format(ProjectFormatError),
    Codec(CodecError),
    AirCodec(AirPayloadError),
    Bridge(crate::daw_project::BridgeError),
    Export(ExportError),
    Envelope(ProjectIoError),
    Workspace(String),
    MissingSection(String),
    MissingPayload(PathBuf),
    UnexpectedAirPayloadCollision(PathBuf),
    MissingGeneratedMediaRoute(AssetId),
    MissingGeneratedPcm(AssetId),
    GeneratedPcm { asset: AssetId, message: String },
}

impl fmt::Display for ProjectRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "project storage: {error}"),
            Self::Format(error) => write!(formatter, "project format: {error}"),
            Self::Codec(error) => write!(formatter, "project codec: {error}"),
            Self::AirCodec(error) => write!(formatter, "AIR codec: {error}"),
            Self::Bridge(error) => write!(formatter, "project bridge: {error}"),
            Self::Export(error) => write!(formatter, "export: {error}"),
            Self::Envelope(error) => write!(formatter, "project envelope: {error}"),
            Self::Workspace(error) => write!(formatter, "workspace migration: {error}"),
            Self::MissingSection(domain) => {
                write!(formatter, "project lacks required {domain} section")
            }
            Self::MissingPayload(path) => {
                write!(formatter, "project lacks payload {}", path.display())
            }
            Self::UnexpectedAirPayloadCollision(path) => write!(
                formatter,
                "constructive codec unexpectedly produced AIR payload {}",
                path.display()
            ),
            Self::MissingGeneratedMediaRoute(asset) => write!(
                formatter,
                "generated asset {} has no project-relative media route",
                asset.0
            ),
            Self::MissingGeneratedPcm(asset) => write!(
                formatter,
                "generated asset {} has no materialized PCM to save",
                asset.0
            ),
            Self::GeneratedPcm { asset, message } => write!(
                formatter,
                "generated asset {} cannot be persisted: {message}",
                asset.0
            ),
        }
    }
}

impl Error for ProjectRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Format(error) => Some(error),
            Self::Codec(error) => Some(error),
            Self::AirCodec(error) => Some(error),
            Self::Bridge(error) => Some(error),
            Self::Export(error) => Some(error),
            Self::Envelope(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::{Frame, FrameRange, SourceRange, TrackKind};
    use crate::assets::{AssetFrameRange, AssetUsageOwner, SampleFrames};
    use crate::daw_engine::{compile_daw_engine, DawEngineConfig};
    use crate::daw_project::ProjectDomain;
    use crate::daw_render::{RenderCancellation, RenderWindow};
    use crate::media_resolver::{RubatoSampleRateConverter, SymphoniaMediaDecoder};
    use crate::mixer::BusKind;
    use crate::ontology::{
        AudioSource, Hypothesis, HypothesisClaim, HypothesisId, Producer, Provenance, SourceId,
    };
    use crate::project_format::ProjectPackage;
    use crate::sample_kit::{SampleKit, SampleKitPut, SampleRouteIntent};
    use crate::sequencer::{
        BeatDuration, PatternContent, PatternDefinition, PatternOrigin, SequencerCommand,
        TriggerTarget, PPQ,
    };
    use crate::workspace_document::WorkspaceDocument;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TempPackage {
        path: PathBuf,
    }

    impl TempPackage {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "audec-project-repository-test-{}-{sequence}.audec",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempPackage {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        assert_eq!(samples.len() % usize::from(channels), 0);
        let data_bytes = u32::try_from(samples.len() * 2).unwrap();
        let block_align = channels * 2;
        let byte_rate = sample_rate * u32::from(block_align);
        let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    /// Test-only injected interpretation codec. The production repository
    /// deliberately does not pretend that an in-memory cache is a durable AIR
    /// format; this verifies that a real codec may own the claim language
    /// without changing package/recovery mechanics.
    #[derive(Clone, Default)]
    struct RecordingAirCodec(Arc<Mutex<Option<AuditoryIr>>>);

    impl AirPayloadCodec for RecordingAirCodec {
        fn encode_air(&self, air: &AuditoryIr) -> Result<Vec<u8>, AirPayloadError> {
            *self.0.lock().unwrap() = Some(air.clone());
            Ok(format!("air-schema-{}", air.schema_version).into_bytes())
        }

        fn decode_air(
            &self,
            _descriptor: &DomainSectionRecord,
            _bytes: &[u8],
        ) -> Result<AuditoryIr, AirPayloadError> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| AirPayloadError::Decoding("test AIR cache is empty".into()))
        }
    }

    fn project_with_interpretation() -> DawProject {
        let mut project = DawProject::new("study", 48_000, 120.0).unwrap();
        project
            .transact(
                "preserve reading",
                project.revisions().aggregate,
                BTreeSet::from([
                    ProjectDomain::Air,
                    ProjectDomain::Mixer,
                    ProjectDomain::Sequencer,
                    ProjectDomain::SampleKits,
                ]),
                |state| -> Result<(), String> {
                    state
                        .domains
                        .air
                        .insert_source(AudioSource {
                            id: SourceId::new(7),
                            uri: "source:like-a-pen".into(),
                            content_digest: Some("fnv1a128:demo".into()),
                            sample_rate: 48_000,
                            channels: 2,
                            frame_count: 48_000,
                        })
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .air
                        .insert_hypothesis(Hypothesis {
                            id: HypothesisId::new(41),
                            label: "reading-qualified possibility".into(),
                            claims: vec![HypothesisClaim::FreeformPerceptualDescription {
                                objects: Vec::new(),
                                description: "portable spectral interpretation".into(),
                            }],
                            support: 0.625,
                            evidence: Vec::new(),
                            provenance: Provenance {
                                producer: Producer::Importer {
                                    format: "audec-reading".into(),
                                    version: "1".into(),
                                },
                                created_unix_ms: None,
                                source_revision: Some(
                                    "reading:00112233445566778899aabbccddeeff:9".into(),
                                ),
                                note: Some(
                                    "foreign:00112233445566778899aabbccddeeff:hypothesis:73".into(),
                                ),
                            },
                        })
                        .map_err(|error| error.to_string())?;
                    let kit_id = state
                        .domains
                        .sample_kits
                        .allocate_kit_id()
                        .map_err(|error| error.to_string())?;
                    let pads_bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "Pads")
                        .map_err(|error| error.to_string())?;
                    let kit = SampleKit::new(
                        kit_id,
                        "Reading pads",
                        SampleRouteIntent::new(pads_bus).map_err(|error| error.to_string())?,
                    );
                    state
                        .domains
                        .sample_kits
                        .apply_puts(&[SampleKitPut {
                            before: None,
                            after: Some(kit),
                        }])
                        .map_err(|error| error.to_string())?;

                    let source = "lead";
                    let bindings = BTreeMap::from([(
                        source.into(),
                        TriggerTarget::InstrumentNote {
                            instrument: 3,
                            key: 60,
                        },
                    )]);
                    let term =
                        crate::pattern_lang::parse(source).map_err(|error| error.to_string())?;
                    let length = BeatDuration(4 * PPQ as u64);
                    let evaluated = crate::pattern_lang::eval_steps(
                        &term,
                        &crate::pattern_lang::EvalContext {
                            bindings: &bindings,
                            cycle: length,
                            seed: 0,
                            cycle_index: 0,
                        },
                    )
                    .map_err(|error| error.to_string())?;
                    let id = state.domains.sequencer.allocate_pattern_id();
                    state
                        .domains
                        .sequencer
                        .execute(
                            "persist expression pattern",
                            vec![SequencerCommand::PutPattern {
                                before: None,
                                after: Some(PatternDefinition {
                                    id,
                                    name: "lead pattern".into(),
                                    length,
                                    content: PatternContent::Steps(evaluated.pattern),
                                    origin: PatternOrigin::Expression {
                                        source: source.into(),
                                        term_hash: crate::pattern_lang::term_hash(&term),
                                        bindings_hash: crate::pattern_lang::bindings_hash(
                                            &bindings,
                                        ),
                                        bindings,
                                        diverged: false,
                                    },
                                    revision: 0,
                                }),
                            }],
                        )
                        .map_err(|error| error.to_string())?;
                    Ok(())
                },
            )
            .unwrap();
        project
    }

    #[test]
    fn empty_air_project_saves_reopens_and_keeps_revision_pinned() {
        let package = TempPackage::new();
        let store = ProjectStore::new(ProjectPackage::new(&package.path).unwrap());
        let repository = ProjectRepository::new(store, EmptyAirPayloadCodec);
        let project = DawProject::new("study", 48_000, 120.0).unwrap();
        let save = repository
            .save_primary(&project, PreservedProjectData::default())
            .unwrap();
        assert_eq!(save.revision_guard.revision, 0);
        let opened = repository.open_primary().unwrap();
        assert_eq!(opened.project.name, "study");
        assert_eq!(opened.project.revisions(), project.revisions());
        assert!(!opened.project.is_dirty());
    }

    #[test]
    fn cross_rate_import_saves_reopens_hydrates_and_renders_one_pcm_identity() {
        let package = TempPackage::new();
        let source_path = package.path.join("source-44k.wav");
        let samples = (0..441)
            .map(|frame| {
                let phase = std::f32::consts::TAU * 440.0 * frame as f32 / 44_100.0;
                (phase.sin() * 16_000.0) as i16
            })
            .collect::<Vec<_>>();
        fs::write(&source_path, pcm16_wav(44_100, 1, &samples)).unwrap();
        let decoder = SymphoniaMediaDecoder::default();
        let imported = decoder
            .materialize_import(
                &source_path,
                "Cross-rate source",
                48_000,
                &RubatoSampleRateConverter::default(),
                42,
                "repository test",
                BTreeSet::from(["imported".into()]),
                false,
            )
            .unwrap();
        assert_eq!(imported.registration.metadata.sample_rate_hz, 48_000);
        assert_eq!(
            imported.registration.metadata.frame_count,
            SampleFrames(480)
        );

        let mut project = DawProject::new("cross-rate", 48_000, 120.0).unwrap();
        let expected_pcm = imported.pcm.clone();
        project
            .transact(
                "place materialized import",
                project.revisions().aggregate,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Assets,
                    ProjectDomain::Bindings,
                    ProjectDomain::Mixer,
                ]),
                |state| -> Result<(), String> {
                    let asset = state
                        .domains
                        .assets
                        .register(imported.registration.clone())
                        .map_err(|error| error.to_string())?;
                    let alias = state
                        .bindings
                        .bind_media_asset(asset)
                        .map_err(|error| error.to_string())?;
                    let mut editor = crate::arrangement::ArrangementEditor::from_state(
                        state.domains.arrangement.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                    let track = editor
                        .create_track("Audio", TrackKind::Audio)
                        .map_err(|error| error.to_string())?;
                    let clip = editor
                        .create_audio_clip(
                            track,
                            "Cross-rate source",
                            FrameRange::new(Frame(0), Frame(480)).unwrap(),
                            alias,
                            SourceRange::new(0, 480).unwrap(),
                        )
                        .map_err(|error| error.to_string())?;
                    state
                        .domains
                        .assets
                        .add_usage(
                            asset,
                            AssetUsageOwner::AudioClip {
                                persistent_id: clip.get(),
                            },
                            Some(AssetFrameRange::new(SampleFrames(0), SampleFrames(480)).unwrap()),
                            "arrangement",
                        )
                        .map_err(|error| error.to_string())?;
                    state.domains.arrangement = editor.state().clone();
                    let bus = state
                        .domains
                        .mixer
                        .add_bus(BusKind::Source, "Audio")
                        .map_err(|error| error.to_string())?;
                    state.bindings.mixer.tracks.insert(track, bus);
                    Ok(())
                },
            )
            .unwrap();

        let repository = ProjectRepository::new(
            ProjectStore::new(ProjectPackage::new(&package.path).unwrap()),
            EmptyAirPayloadCodec,
        );
        repository
            .save_primary(&project, PreservedProjectData::default())
            .unwrap();
        let reopened = repository.open_primary().unwrap();
        let hydration = repository.hydrate_media(&reopened.project, &decoder);
        assert!(
            hydration.unresolved_assets.is_empty(),
            "{:?}",
            hydration.diagnostics
        );
        assert_eq!(hydration.pcm.len(), 1);
        let hydrated = hydration.pcm.values().next().unwrap();
        assert_eq!(hydrated.format.sample_rate.get(), 48_000);
        assert_eq!(hydrated.samples.as_ref(), expected_pcm.samples.as_ref());

        let cancellation = RenderCancellation::new();
        let window = RenderWindow::new(0, 480).unwrap();
        let schedule = compile_daw_engine(
            &reopened.project,
            &hydration.pcm,
            window,
            &DawEngineConfig::default(),
            &cancellation,
        )
        .unwrap();
        let rendered = schedule.render(window, &cancellation).unwrap();
        assert!(rendered.engine_diagnostics.is_empty());
        assert!(rendered
            .audio
            .interleaved()
            .iter()
            .any(|sample| sample.abs() > 0.01));
    }

    #[test]
    fn unknown_payload_survives_repository_save_and_open() {
        let package = TempPackage::new();
        let store = ProjectStore::new(ProjectPackage::new(&package.path).unwrap());
        let repository = ProjectRepository::new(store, EmptyAirPayloadCodec);
        let project = DawProject::new("study", 48_000, 120.0).unwrap();
        let preserved = PreservedProjectData {
            envelope_extensions: BTreeMap::from([(
                "vendor_note".into(),
                serde_json::Value::String("retain me".into()),
            )]),
            sections: BTreeMap::from([(
                "vendor.claims".into(),
                crate::project_format::PreservedSection {
                    descriptor: DomainSectionRecord {
                        domain: "vendor.claims".into(),
                        schema_version: 8,
                        revision: 0,
                        payload_key: "vendor.claims.bin".into(),
                        encoding: "binary".into(),
                    },
                    bytes: vec![7, 0, 9],
                },
            )]),
        };
        repository.save_primary(&project, preserved).unwrap();
        let opened = repository.open_primary().unwrap();
        assert_eq!(
            opened.preserved.sections["vendor.claims"].bytes,
            vec![7, 0, 9]
        );
        assert_eq!(
            opened.preserved.envelope_extensions["vendor_note"],
            serde_json::Value::String("retain me".into())
        );
        repository
            .save_primary(&opened.project, opened.preserved.clone())
            .unwrap();
        let reopened = repository.open_primary().unwrap();
        assert_eq!(
            reopened.preserved.sections["vendor.claims"].bytes,
            vec![7, 0, 9]
        );
    }

    #[test]
    fn autosave_is_discoverable_and_only_opened_by_explicit_choice() {
        let package = TempPackage::new();
        let store = ProjectStore::new(ProjectPackage::new(&package.path).unwrap());
        let repository = ProjectRepository::new(store, EmptyAirPayloadCodec);
        let project = DawProject::new("study", 48_000, 120.0).unwrap();

        repository
            .save_autosave(&project, PreservedProjectData::default(), 1_735_000_000_000)
            .unwrap();
        let discovery = repository.recovery_discovery();
        assert!(discovery.primary.is_none());
        assert_eq!(discovery.checkpoints.len(), 1);
        assert_eq!(discovery.checkpoints[0].base_project_revision, 0);

        let recovered = repository.open_recovery(&discovery.checkpoints[0]).unwrap();
        assert_eq!(recovered.project.name, "study");
        assert!(!recovered.project.is_dirty());
    }

    #[test]
    fn complete_project_and_dynamic_workspace_round_trip() {
        let package = TempPackage::new();
        let store = ProjectStore::new(ProjectPackage::new(&package.path).unwrap());
        let repository = ProjectRepository::new(store, RecordingAirCodec::default());
        let project = project_with_interpretation();
        let workspace = WorkspaceDocument::default();

        let saved = repository
            .save_primary_with_workspace(
                &project,
                Some(&workspace),
                PreservedProjectData::default(),
            )
            .unwrap();
        let opened = repository.open_primary().unwrap();

        assert_eq!(saved.revision_guard.revision, project.revisions().aggregate);
        assert_eq!(
            project_codecs::encode_constructive(&opened.project).unwrap(),
            project_codecs::encode_constructive(&project).unwrap()
        );
        assert_eq!(
            opened.project.state().domains.air,
            project.state().domains.air
        );
        assert_eq!(opened.project.revisions(), project.revisions());
        assert_eq!(opened.workspace, Some(workspace));
        assert!(!opened.project.is_dirty());

        // Opening then saving the durable workspace again must not collide
        // with the extension copy retained for forward compatibility.
        repository
            .save_primary_with_workspace(
                &opened.project,
                opened.workspace.as_ref(),
                opened.preserved.clone(),
            )
            .unwrap();
        assert_eq!(
            repository.open_primary().unwrap().workspace,
            opened.workspace
        );
    }

    #[test]
    fn newer_interrupted_autosave_is_offered_but_never_auto_restored() {
        let package = TempPackage::new();
        let store = ProjectStore::new(ProjectPackage::new(&package.path).unwrap());
        let repository = ProjectRepository::new(store, RecordingAirCodec::default());
        let primary = DawProject::new("study", 48_000, 120.0).unwrap();
        repository
            .save_primary(&primary, PreservedProjectData::default())
            .unwrap();
        let changed = project_with_interpretation();
        repository
            .save_autosave(&changed, PreservedProjectData::default(), 1_735_000_000_000)
            .unwrap();

        let preference = repository.recovery_preference().unwrap();
        let RecoveryPreference::OfferRecovery {
            primary_revision,
            recovery,
        } = preference
        else {
            panic!("newer autosave should be offered")
        };
        assert_eq!(primary_revision, Some(0));
        assert_eq!(
            recovery.base_project_revision,
            changed.revisions().aggregate
        );
        let restored = repository.open_recovery(&recovery).unwrap();
        assert_eq!(
            project_codecs::encode_constructive(&restored.project).unwrap(),
            project_codecs::encode_constructive(&changed).unwrap()
        );
        assert_eq!(
            restored.project.state().domains.air,
            changed.state().domains.air
        );
    }

    #[test]
    fn empty_air_codec_refuses_nonempty_interpretation() {
        let package = TempPackage::new();
        let store = ProjectStore::new(ProjectPackage::new(&package.path).unwrap());
        let repository = ProjectRepository::new(store, EmptyAirPayloadCodec);
        let error = repository
            .save_primary(
                &project_with_interpretation(),
                PreservedProjectData::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectRepositoryError::AirCodec(AirPayloadError::NonEmptyAirRequiresCodec)
        ));
    }

    fn air_descriptor() -> DomainSectionRecord {
        DomainSectionRecord {
            domain: "air".into(),
            schema_version: AuditoryIr::CURRENT_SCHEMA_VERSION,
            revision: 4,
            payload_key: "air.json".into(),
            encoding: project_codecs::JSON_ENCODING.into(),
        }
    }

    #[test]
    fn deterministic_air_codec_round_trips_nonempty_reading_provenance() {
        let air = project_with_interpretation().state().domains.air.clone();
        let encoded = JsonAirPayloadCodec.encode_air(&air).unwrap();
        assert_eq!(
            encoded,
            JsonAirPayloadCodec.encode_air(&air).unwrap(),
            "the same AIR graph must produce byte-identical payloads"
        );

        let decoded = JsonAirPayloadCodec
            .decode_air(&air_descriptor(), &encoded)
            .unwrap();
        assert_eq!(decoded, air);
        assert_eq!(
            decoded.hypotheses[&HypothesisId::new(41)]
                .provenance
                .note
                .as_deref(),
            Some("foreign:00112233445566778899aabbccddeeff:hypothesis:73")
        );
    }

    #[test]
    fn production_repository_reopens_nonempty_air_with_a_fresh_codec_instance() {
        let package = TempPackage::new();
        let project = project_with_interpretation();
        ProjectRepository::new(
            ProjectStore::new(ProjectPackage::new(&package.path).unwrap()),
            JsonAirPayloadCodec,
        )
        .save_primary(&project, PreservedProjectData::default())
        .unwrap();

        let reopened = ProjectRepository::new(
            ProjectStore::new(ProjectPackage::new(&package.path).unwrap()),
            JsonAirPayloadCodec,
        )
        .open_primary()
        .unwrap();
        assert_eq!(
            reopened.project.state().domains.air,
            project.state().domains.air
        );
        assert_eq!(reopened.project.revisions(), project.revisions());
    }

    #[test]
    fn production_air_codec_round_trips_f32_measurements_losslessly() {
        use crate::ontology::{
            AudioSource, ChannelSelection, Evidence, EvidenceId, EvidenceKind, MeasurementValue,
            Producer, Provenance, SampleRange, SourceId, SourceSpan, SpanId,
        };
        // Retained analysis stores f32 strengths and feature vectors whose
        // JSON text re-parses as f64; the lossless check must not mistake that
        // for a dropped field.
        let mut air = AuditoryIr::new(44_100);
        air.insert_source(AudioSource {
            id: SourceId::new(1),
            uri: "file:///material.flac".into(),
            content_digest: None,
            sample_rate: 44_100,
            channels: 2,
            frame_count: 16_468_704,
        })
        .unwrap();
        air.insert_span(SourceSpan {
            id: SpanId::new(1),
            source: SourceId::new(1),
            range: SampleRange::new(17_384, 22_000).unwrap(),
            channels: ChannelSelection::All,
        })
        .unwrap();
        air.insert_evidence(Evidence {
            id: EvidenceId::new(1),
            kind: EvidenceKind::SourceMeasurement {
                spans: vec![SpanId::new(1)],
                feature: "component magnitude".into(),
                value: MeasurementValue::Vector(vec![0.086_122_945, 0.222_858_56, 1.0, 0.0]),
            },
            strength: 0.222_858_56,
            provenance: Provenance {
                producer: Producer::Analyzer {
                    name: "nmf".into(),
                    version: "1".into(),
                    configuration_digest: None,
                },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
        })
        .unwrap();
        let encoded = JsonAirPayloadCodec.encode_air(&air).unwrap();
        let decoded = JsonAirPayloadCodec
            .decode_air(&air_descriptor(), &encoded)
            .expect("f32 measurements survive the typed round trip");
        assert_eq!(decoded, air);
        // Structure is still checked: a dropped key is refused.
        let mut pruned: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        pruned["evidence"]["1"]
            .as_object_mut()
            .unwrap()
            .insert("future".into(), serde_json::json!(1));
        assert!(JsonAirPayloadCodec
            .decode_air(&air_descriptor(), &serde_json::to_vec(&pruned).unwrap())
            .is_err());
    }

    #[test]
    fn production_air_codec_reads_legacy_empty_payloads() {
        let air = AuditoryIr::new(48_000);
        let bytes = EmptyAirPayloadCodec.encode_air(&air).unwrap();
        assert_eq!(
            JsonAirPayloadCodec
                .decode_air(&air_descriptor(), &bytes)
                .unwrap(),
            air
        );
    }

    #[test]
    fn production_air_codec_refuses_duplicate_unknown_and_invalid_identity_data() {
        let air = project_with_interpretation().state().domains.air.clone();
        let encoded = String::from_utf8(JsonAirPayloadCodec.encode_air(&air).unwrap()).unwrap();
        let duplicate = encoded.replacen(
            "\"sample_rate\": 48000,",
            "\"sample_rate\": 48000,\n  \"sample_rate\": 48000,",
            1,
        );
        let error = JsonAirPayloadCodec
            .decode_air(&air_descriptor(), duplicate.as_bytes())
            .unwrap_err();
        assert!(error.to_string().contains("duplicate JSON object key"));

        let mut unknown: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("future_claims".into(), serde_json::json!({"opaque": true}));
        let error = JsonAirPayloadCodec
            .decode_air(
                &air_descriptor(),
                &serde_json::to_vec_pretty(&unknown).unwrap(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("cannot preserve losslessly"));

        let mut mismatched: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        mismatched["sources"]["7"]["id"] = serde_json::json!(8);
        let error = JsonAirPayloadCodec
            .decode_air(
                &air_descriptor(),
                &serde_json::to_vec_pretty(&mismatched).unwrap(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("map key and embedded id differ"));
    }
}
