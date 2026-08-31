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

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::assets::AssetId;
use crate::daw_engine::AssetPcmMap;
use crate::daw_project::{DawProject, ProjectRevisions, DAW_PROJECT_SCHEMA_VERSION};
use crate::export::{
    export_revision_pinned_audio_to_wav, ExportError, ExportObserver, RevisionPinnedAudio,
    RevisionPinnedWavExportReport, WavExportRequest,
};
use crate::media_resolver::{
    resolve_material, MaterialRequest, MaterialResolution, MediaDecoder, RelinkProposal,
    ResolutionDiagnostic,
};
use crate::ontology::AuditoryIr;
use crate::project_codecs::{self, CodecError};
use crate::project_format::{PreservedProjectData, ProjectCheckpoint, ProjectFormatError};
use crate::project_io::{DomainSectionRecord, ProjectFile, ProjectIoDiagnostic};
use crate::project_store::{
    LoadedCheckpoint, ProjectStore, ProjectStoreError, RecoveryCheckpoint, RecoveryDiscovery,
    SaveResult,
};

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

    /// Build and atomically publish a primary checkpoint. `preserved` must be
    /// carried from a prior open when saving an older build's project, so
    /// unknown payloads and envelope fields survive untouched.
    pub fn save_primary(
        &self,
        project: &DawProject,
        preserved: PreservedProjectData,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        let revision = project.revisions().aggregate;
        let checkpoint = self.build_checkpoint(project, preserved)?;
        self.store
            .save_primary(&checkpoint, revision)
            .map_err(ProjectRepositoryError::Store)
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
        let checkpoint = self.build_checkpoint(project, preserved)?;
        self.store
            .save_autosave(&checkpoint, revision, saved_unix_ms)
            .map_err(ProjectRepositoryError::Store)
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
                asset.metadata().clone(),
                asset.content(),
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
                    hydration.pcm.insert(id, resolved.decoded.pcm);
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
        preserved: PreservedProjectData,
    ) -> Result<ProjectCheckpoint, ProjectRepositoryError> {
        let file = ProjectFile::from_project(project, None);
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
        Ok(OpenedProject {
            project,
            preserved,
            diagnostics,
            manifest_path: loaded.manifest_path,
        })
    }
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
    MissingSection(String),
    MissingPayload(PathBuf),
    UnexpectedAirPayloadCollision(PathBuf),
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
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_format::ProjectPackage;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

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
}
