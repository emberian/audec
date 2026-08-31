//! UI-neutral durable lifecycle for one authoritative [`ProjectSession`].
//!
//! Open, save, recovery, and export are split into capture/worker/completion
//! phases. No worker retains a `LiveProject`, and completing a job only ever
//! installs or marks the one controller owned by the session.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::analysis::Analysis;
use crate::assets::AssetId;
use crate::audio::ProjectAudio;
use crate::daw_project::{DawProject, ProjectRevisions};
use crate::export::{
    ExportObserver, RevisionPinnedAudio, RevisionPinnedWavExportReport, WavExportRequest,
};
use crate::file_actions::ProjectFileActions;
use crate::live_project::{LiveProject, LiveProjectError};
use crate::media_resolver::{MediaDecoder, RelinkProposal};
use crate::project_format::PreservedProjectData;
use crate::project_io::ProjectIoDiagnostic;
use crate::project_repository::{
    AirPayloadCodec, MediaHydrationDiagnostic, OpenedProject, ProjectRepositoryError,
};
use crate::project_store::{RecoveryCheckpoint, RecoveryDiscovery, SaveResult, StoreDiagnostic};
use crate::workspace_document::WorkspaceDocument;

use super::{ProjectSession, ProjectSessionError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectDocumentOrigin {
    Primary,
    Recovery(RecoveryCheckpoint),
}

/// Typed diagnostics retained for the lifetime of an opened document. Missing
/// media and relink candidates are deliberately not collapsed into strings.
#[derive(Clone, Debug, Default)]
pub struct ProjectDocumentDiagnostics {
    pub project_io: Vec<ProjectIoDiagnostic>,
    pub media: Vec<MediaHydrationDiagnostic>,
    pub unresolved_assets: Vec<AssetId>,
    pub relink_proposals: Vec<RelinkProposal>,
    pub recovery_store: Vec<StoreDiagnostic>,
}

impl ProjectDocumentDiagnostics {
    fn summaries(&self) -> Vec<String> {
        self.project_io
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .chain(
                self.media
                    .iter()
                    .map(|diagnostic| diagnostic.message.clone()),
            )
            .chain(
                self.recovery_store
                    .iter()
                    .map(|diagnostic| diagnostic.message.clone()),
            )
            .collect()
    }
}

/// Persistence state paired with one application-owned [`ProjectSession`].
/// This is not a second project model: all editable state and history remain
/// in the session passed to each lifecycle boundary.
pub struct ProjectDocumentLifecycle<C> {
    files: Option<ProjectFileActions<C>>,
    manifest_path: Option<PathBuf>,
    origin: Option<ProjectDocumentOrigin>,
    preserved: PreservedProjectData,
    workspace: Option<WorkspaceDocument>,
    workspace_revision: u64,
    saved_workspace_revision: u64,
    diagnostics: ProjectDocumentDiagnostics,
    recovery: RecoveryDiscovery,
    document_epoch: u64,
    operation_sequence: u64,
    pending_open: Option<u64>,
    latest_primary_save: Option<u64>,
}

impl<C> ProjectDocumentLifecycle<C>
where
    C: AirPayloadCodec + Clone,
{
    pub fn new() -> Self {
        Self {
            files: None,
            manifest_path: None,
            origin: None,
            preserved: PreservedProjectData::default(),
            workspace: None,
            workspace_revision: 0,
            saved_workspace_revision: 0,
            diagnostics: ProjectDocumentDiagnostics::default(),
            recovery: RecoveryDiscovery::default(),
            document_epoch: 0,
            operation_sequence: 0,
            pending_open: None,
            latest_primary_save: None,
        }
    }

    pub fn manifest_path(&self) -> Option<&Path> {
        self.manifest_path.as_deref()
    }

    pub fn origin(&self) -> Option<&ProjectDocumentOrigin> {
        self.origin.as_ref()
    }

    pub fn preserved(&self) -> &PreservedProjectData {
        &self.preserved
    }

    pub fn workspace(&self) -> Option<&WorkspaceDocument> {
        self.workspace.as_ref()
    }

    pub fn diagnostics(&self) -> &ProjectDocumentDiagnostics {
        &self.diagnostics
    }

    pub fn recovery_options(&self) -> &RecoveryDiscovery {
        &self.recovery
    }

    pub fn is_dirty(&self, session: &ProjectSession) -> Result<bool, ProjectLifecycleError> {
        Ok(matches!(
            self.origin.as_ref(),
            Some(ProjectDocumentOrigin::Recovery(_))
        ) || session.is_dirty()?
            || self.workspace_revision != self.saved_workspace_revision)
    }

    pub fn replace_workspace(&mut self, workspace: Option<WorkspaceDocument>) -> bool {
        if self.workspace == workspace {
            return false;
        }
        self.workspace = workspace;
        self.workspace_revision = self.workspace_revision.wrapping_add(1);
        true
    }

    /// Start an ordinary open. The returned request owns only filesystem
    /// services and can be moved to a worker before `finish_open` is called.
    pub fn begin_open_primary(
        &mut self,
        session: &mut ProjectSession,
        files: ProjectFileActions<C>,
    ) -> ProjectOpenRequest<C> {
        self.begin_open(session, files, ProjectDocumentOrigin::Primary)
    }

    /// Recovery is impossible without passing a checkpoint chosen by the UI.
    pub fn begin_open_recovery(
        &mut self,
        session: &mut ProjectSession,
        files: ProjectFileActions<C>,
        checkpoint: RecoveryCheckpoint,
    ) -> ProjectOpenRequest<C> {
        self.begin_open(session, files, ProjectDocumentOrigin::Recovery(checkpoint))
    }

    fn begin_open(
        &mut self,
        session: &mut ProjectSession,
        files: ProjectFileActions<C>,
        origin: ProjectDocumentOrigin,
    ) -> ProjectOpenRequest<C> {
        let token = self.next_operation();
        let source = match &origin {
            ProjectDocumentOrigin::Primary => files.repository().store().package().manifest_path(),
            ProjectDocumentOrigin::Recovery(checkpoint) => checkpoint.manifest_path.clone(),
        };
        session.begin_loading(source);
        session.replace_diagnostics(Vec::new());
        self.files = None;
        self.manifest_path = None;
        self.origin = None;
        self.preserved = PreservedProjectData::default();
        self.workspace = None;
        self.diagnostics = ProjectDocumentDiagnostics::default();
        self.recovery = RecoveryDiscovery::default();
        self.pending_open = Some(token);
        self.latest_primary_save = None;
        ProjectOpenRequest {
            token,
            files,
            origin,
        }
    }

    pub fn finish_open(
        &mut self,
        session: &mut ProjectSession,
        completion: ProjectOpenCompletion<C>,
        analysis: Option<Arc<Analysis>>,
    ) -> Result<ProjectOpenOutcome, ProjectLifecycleError> {
        if self.pending_open != Some(completion.token) {
            return Err(ProjectLifecycleError::StaleOpenCompletion);
        }
        self.pending_open = None;
        let loaded = match completion.loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                session.fail(error.to_string());
                return Err(ProjectLifecycleError::Repository(error));
            }
        };
        let OpenedProject {
            project,
            workspace,
            preserved,
            diagnostics: project_io,
            manifest_path,
        } = loaded.opened;
        let live = match LiveProject::from_project(project, loaded.hydration.pcm) {
            Ok(live) => live,
            Err(error) => {
                session.fail(error.to_string());
                return Err(ProjectLifecycleError::LiveProject(error));
            }
        };
        let revisions = match session.install(live, analysis) {
            Ok(revisions) => revisions,
            Err(error) => {
                session.fail(error.to_string());
                return Err(ProjectLifecycleError::Session(error));
            }
        };
        let diagnostics = ProjectDocumentDiagnostics {
            project_io,
            media: loaded.hydration.diagnostics,
            unresolved_assets: loaded.hydration.unresolved_assets,
            relink_proposals: loaded.hydration.relink_proposals,
            recovery_store: loaded.recovery.diagnostics.clone(),
        };
        session.replace_diagnostics(diagnostics.summaries());
        self.files = Some(completion.files);
        self.manifest_path = Some(manifest_path.clone());
        self.origin = Some(completion.origin.clone());
        self.preserved = preserved;
        self.workspace = workspace;
        self.workspace_revision = 0;
        self.saved_workspace_revision = 0;
        self.diagnostics = diagnostics;
        self.recovery = loaded.recovery;
        self.document_epoch = self.document_epoch.wrapping_add(1);
        if self.document_epoch == 0 {
            self.document_epoch = 1;
        }
        Ok(ProjectOpenOutcome {
            revisions,
            origin: completion.origin,
            manifest_path,
            recovery_available: self.recovery.checkpoints.len(),
        })
    }

    /// Capture an exact aggregate/workspace pair for the currently opened
    /// package. Persistence can complete after later edits without clearing
    /// their dirty state.
    pub fn begin_save(
        &mut self,
        session: &ProjectSession,
    ) -> Result<ProjectSaveRequest<C>, ProjectLifecycleError> {
        let files = self
            .files
            .clone()
            .ok_or(ProjectLifecycleError::NoRepository)?;
        self.begin_primary_save(session, files)
    }

    /// Save As differs only by its destination repository. On successful
    /// completion it becomes the document's current package even if a raced
    /// edit means the document remains dirty.
    pub fn begin_save_as(
        &mut self,
        session: &ProjectSession,
        files: ProjectFileActions<C>,
    ) -> Result<ProjectSaveRequest<C>, ProjectLifecycleError> {
        self.begin_primary_save(session, files)
    }

    fn begin_primary_save(
        &mut self,
        session: &ProjectSession,
        files: ProjectFileActions<C>,
    ) -> Result<ProjectSaveRequest<C>, ProjectLifecycleError> {
        let token = self.next_operation();
        self.latest_primary_save = Some(token);
        self.capture_save(session, files, token, SaveKind::Primary)
    }

    pub fn begin_autosave(
        &mut self,
        session: &ProjectSession,
        saved_unix_ms: u64,
    ) -> Result<ProjectSaveRequest<C>, ProjectLifecycleError> {
        let files = self
            .files
            .clone()
            .ok_or(ProjectLifecycleError::NoRepository)?;
        let token = self.next_operation();
        self.capture_save(session, files, token, SaveKind::Autosave { saved_unix_ms })
    }

    fn capture_save(
        &self,
        session: &ProjectSession,
        files: ProjectFileActions<C>,
        token: u64,
        kind: SaveKind,
    ) -> Result<ProjectSaveRequest<C>, ProjectLifecycleError> {
        let snapshot = session.project_snapshot()?;
        Ok(ProjectSaveRequest {
            token,
            document_epoch: self.document_epoch,
            files,
            kind,
            project: Arc::clone(&snapshot.project),
            workspace: self.workspace.clone(),
            workspace_revision: self.workspace_revision,
            preserved: self.preserved.clone(),
        })
    }

    pub fn finish_save(
        &mut self,
        session: &mut ProjectSession,
        completion: ProjectSaveCompletion<C>,
    ) -> Result<ProjectSaveOutcome, ProjectLifecycleError> {
        if completion.document_epoch != self.document_epoch {
            return Err(ProjectLifecycleError::DocumentChangedDuringOperation);
        }
        if completion.kind == SaveKind::Primary
            && self.latest_primary_save != Some(completion.token)
        {
            return Err(ProjectLifecycleError::SupersededSaveCompletion);
        }
        let result = completion
            .result
            .map_err(ProjectLifecycleError::Repository)?;
        match completion.kind {
            SaveKind::Autosave { .. } => Ok(ProjectSaveOutcome {
                result,
                project_marked_saved: false,
                workspace_marked_saved: false,
                document_clean: self.is_dirty(session).map(|dirty| !dirty)?,
            }),
            SaveKind::Primary => {
                self.latest_primary_save = None;
                let recovery = completion.files.recovery_options();
                self.files = Some(completion.files);
                self.manifest_path = Some(result.manifest_path.clone());
                self.origin = Some(ProjectDocumentOrigin::Primary);
                self.diagnostics.recovery_store = recovery.diagnostics.clone();
                self.recovery = recovery;
                session.replace_diagnostics(self.diagnostics.summaries());
                let project_marked_saved =
                    session.mark_saved_if_revision(result.revision_guard.revision)?;
                let workspace_marked_saved =
                    completion.workspace_revision == self.workspace_revision;
                if workspace_marked_saved {
                    self.saved_workspace_revision = completion.workspace_revision;
                }
                Ok(ProjectSaveOutcome {
                    result,
                    project_marked_saved,
                    workspace_marked_saved,
                    document_clean: !self.is_dirty(session)?,
                })
            }
        }
    }

    /// Capture export material only when the audible render and authoritative
    /// aggregate name the same revision. Later edits do not relabel the
    /// already-captured export; its report remains honestly historical.
    pub fn begin_export(
        &self,
        session: &ProjectSession,
        audible_revision: u64,
        audio: ProjectAudio,
        request: WavExportRequest,
    ) -> Result<ProjectExportRequest<C>, ProjectLifecycleError> {
        let project_revision = session.project_snapshot()?.revisions().aggregate;
        if audible_revision != project_revision {
            return Err(ProjectLifecycleError::ExportRevisionConflict {
                project_revision,
                audible_revision,
            });
        }
        let files = self
            .files
            .clone()
            .ok_or(ProjectLifecycleError::NoRepository)?;
        Ok(ProjectExportRequest {
            files,
            pinned: RevisionPinnedAudio::new(project_revision, audio),
            request,
        })
    }

    fn next_operation(&mut self) -> u64 {
        self.operation_sequence = self.operation_sequence.wrapping_add(1);
        if self.operation_sequence == 0 {
            self.operation_sequence = 1;
        }
        self.operation_sequence
    }
}

impl<C> Default for ProjectDocumentLifecycle<C>
where
    C: AirPayloadCodec + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveKind {
    Primary,
    Autosave { saved_unix_ms: u64 },
}

pub struct ProjectOpenRequest<C> {
    token: u64,
    files: ProjectFileActions<C>,
    origin: ProjectDocumentOrigin,
}

impl<C> ProjectOpenRequest<C>
where
    C: AirPayloadCodec,
{
    pub fn load(self, decoder: &impl MediaDecoder) -> ProjectOpenCompletion<C> {
        let opened = match &self.origin {
            ProjectDocumentOrigin::Primary => self.files.open(),
            ProjectDocumentOrigin::Recovery(checkpoint) => self.files.open_recovery(checkpoint),
        };
        let loaded = opened.map(|opened| {
            let hydration = self.files.hydrate(&opened.project, decoder);
            let recovery = self.files.recovery_options();
            LoadedDocument {
                opened,
                hydration,
                recovery,
            }
        });
        ProjectOpenCompletion {
            token: self.token,
            files: self.files,
            origin: self.origin,
            loaded,
        }
    }
}

pub struct ProjectOpenCompletion<C> {
    token: u64,
    files: ProjectFileActions<C>,
    origin: ProjectDocumentOrigin,
    loaded: Result<LoadedDocument, ProjectRepositoryError>,
}

struct LoadedDocument {
    opened: OpenedProject,
    hydration: crate::project_repository::MediaHydration,
    recovery: RecoveryDiscovery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectOpenOutcome {
    pub revisions: ProjectRevisions,
    pub origin: ProjectDocumentOrigin,
    pub manifest_path: PathBuf,
    pub recovery_available: usize,
}

pub struct ProjectSaveRequest<C> {
    token: u64,
    document_epoch: u64,
    files: ProjectFileActions<C>,
    kind: SaveKind,
    project: Arc<DawProject>,
    workspace: Option<WorkspaceDocument>,
    workspace_revision: u64,
    preserved: PreservedProjectData,
}

impl<C> ProjectSaveRequest<C>
where
    C: AirPayloadCodec,
{
    pub fn aggregate_revision(&self) -> u64 {
        self.project.revisions().aggregate
    }

    pub fn persist(self) -> ProjectSaveCompletion<C> {
        let result = match self.kind {
            SaveKind::Primary => self.files.save_with_workspace(
                self.project.as_ref(),
                self.workspace.as_ref(),
                self.preserved,
            ),
            SaveKind::Autosave { saved_unix_ms } => self.files.autosave_with_workspace(
                self.project.as_ref(),
                self.workspace.as_ref(),
                self.preserved,
                saved_unix_ms,
            ),
        };
        ProjectSaveCompletion {
            token: self.token,
            document_epoch: self.document_epoch,
            files: self.files,
            kind: self.kind,
            workspace_revision: self.workspace_revision,
            result,
        }
    }
}

pub struct ProjectSaveCompletion<C> {
    token: u64,
    document_epoch: u64,
    files: ProjectFileActions<C>,
    kind: SaveKind,
    workspace_revision: u64,
    result: Result<SaveResult, ProjectRepositoryError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSaveOutcome {
    pub result: SaveResult,
    pub project_marked_saved: bool,
    pub workspace_marked_saved: bool,
    pub document_clean: bool,
}

pub struct ProjectExportRequest<C> {
    files: ProjectFileActions<C>,
    pinned: RevisionPinnedAudio,
    request: WavExportRequest,
}

impl<C> ProjectExportRequest<C>
where
    C: AirPayloadCodec,
{
    pub fn aggregate_revision(&self) -> u64 {
        self.pinned.aggregate_revision
    }

    pub fn export<O: ExportObserver>(
        self,
        observer: &mut O,
    ) -> Result<RevisionPinnedWavExportReport, ProjectLifecycleError> {
        self.files
            .export(self.pinned, &self.request, observer)
            .map_err(ProjectLifecycleError::Repository)
    }
}

#[derive(Debug)]
pub enum ProjectLifecycleError {
    Session(ProjectSessionError),
    Repository(ProjectRepositoryError),
    LiveProject(LiveProjectError),
    NoRepository,
    StaleOpenCompletion,
    SupersededSaveCompletion,
    DocumentChangedDuringOperation,
    ExportRevisionConflict {
        project_revision: u64,
        audible_revision: u64,
    },
}

impl fmt::Display for ProjectLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::Repository(error) => error.fmt(formatter),
            Self::LiveProject(error) => error.fmt(formatter),
            Self::NoRepository => formatter.write_str("the document has no opened repository"),
            Self::StaleOpenCompletion => {
                formatter.write_str("an older open completed after another open began")
            }
            Self::SupersededSaveCompletion => {
                formatter.write_str("an older save completed after another save began")
            }
            Self::DocumentChangedDuringOperation => {
                formatter.write_str("a file operation completed for a replaced document")
            }
            Self::ExportRevisionConflict {
                project_revision,
                audible_revision,
            } => write!(
                formatter,
                "export audio revision {audible_revision} does not match project revision {project_revision}"
            ),
        }
    }
}

impl Error for ProjectLifecycleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Repository(error) => Some(error),
            Self::LiveProject(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProjectSessionError> for ProjectLifecycleError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error)
    }
}

impl From<LiveProjectError> for ProjectLifecycleError {
    fn from(error: LiveProjectError) -> Self {
        Self::LiveProject(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::command::{claims_for_commands, CommandEnvelope, DomainCommand};
    use crate::mixer::{BusKind, MixerCommand};
    use crate::project_format::{PreservedSection, ProjectPackage, PACKAGE_MANIFEST_NAME};
    use crate::project_io::DomainSectionRecord;
    use crate::project_repository::{EmptyAirPayloadCodec, ProjectRepository};
    use crate::project_store::ProjectStore;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TempPackage {
        path: PathBuf,
    }

    impl TempPackage {
        fn new(label: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "audec-session-lifecycle-{label}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Self { path }
        }

        fn actions(&self) -> ProjectFileActions<EmptyAirPayloadCodec> {
            let package = ProjectPackage::new(&self.path).unwrap();
            ProjectFileActions::new(ProjectRepository::new(
                ProjectStore::new(package),
                EmptyAirPayloadCodec,
            ))
        }
    }

    impl Drop for TempPackage {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct TestDocument {
        lifecycle: ProjectDocumentLifecycle<EmptyAirPayloadCodec>,
        session: ProjectSession,
    }

    impl TestDocument {
        fn new(id: u64) -> Self {
            Self {
                lifecycle: ProjectDocumentLifecycle::new(),
                session: ProjectSession::new(super::super::ProjectSessionId(id)).unwrap(),
            }
        }

        fn session(&self) -> &ProjectSession {
            &self.session
        }

        fn session_mut(&mut self) -> &mut ProjectSession {
            &mut self.session
        }

        fn is_dirty(&self) -> Result<bool, ProjectLifecycleError> {
            self.lifecycle.is_dirty(&self.session)
        }

        fn begin_open_primary(
            &mut self,
            files: ProjectFileActions<EmptyAirPayloadCodec>,
        ) -> ProjectOpenRequest<EmptyAirPayloadCodec> {
            self.lifecycle.begin_open_primary(&mut self.session, files)
        }

        fn begin_open_recovery(
            &mut self,
            files: ProjectFileActions<EmptyAirPayloadCodec>,
            checkpoint: RecoveryCheckpoint,
        ) -> ProjectOpenRequest<EmptyAirPayloadCodec> {
            self.lifecycle
                .begin_open_recovery(&mut self.session, files, checkpoint)
        }

        fn finish_open(
            &mut self,
            completion: ProjectOpenCompletion<EmptyAirPayloadCodec>,
            analysis: Option<Arc<Analysis>>,
        ) -> Result<ProjectOpenOutcome, ProjectLifecycleError> {
            self.lifecycle
                .finish_open(&mut self.session, completion, analysis)
        }

        fn begin_save(
            &mut self,
        ) -> Result<ProjectSaveRequest<EmptyAirPayloadCodec>, ProjectLifecycleError> {
            self.lifecycle.begin_save(&self.session)
        }

        fn begin_save_as(
            &mut self,
            files: ProjectFileActions<EmptyAirPayloadCodec>,
        ) -> Result<ProjectSaveRequest<EmptyAirPayloadCodec>, ProjectLifecycleError> {
            self.lifecycle.begin_save_as(&self.session, files)
        }

        fn finish_save(
            &mut self,
            completion: ProjectSaveCompletion<EmptyAirPayloadCodec>,
        ) -> Result<ProjectSaveOutcome, ProjectLifecycleError> {
            self.lifecycle.finish_save(&mut self.session, completion)
        }

        fn begin_export(
            &self,
            audible_revision: u64,
            audio: ProjectAudio,
            request: WavExportRequest,
        ) -> Result<ProjectExportRequest<EmptyAirPayloadCodec>, ProjectLifecycleError> {
            self.lifecycle
                .begin_export(&self.session, audible_revision, audio, request)
        }
    }

    impl Deref for TestDocument {
        type Target = ProjectDocumentLifecycle<EmptyAirPayloadCodec>;

        fn deref(&self) -> &Self::Target {
            &self.lifecycle
        }
    }

    impl DerefMut for TestDocument {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.lifecycle
        }
    }

    struct MissingDecoder;

    impl MediaDecoder for MissingDecoder {
        fn decode(
            &self,
            path: &Path,
        ) -> Result<crate::media_resolver::DecodedMaterial, crate::media_resolver::MediaDecodeError>
        {
            Err(crate::media_resolver::MediaDecodeError::Io(format!(
                "missing {}",
                path.display()
            )))
        }
    }

    fn preserved() -> PreservedProjectData {
        PreservedProjectData {
            envelope_extensions: BTreeMap::from([(
                "vendor.note".into(),
                serde_json::Value::String("keep".into()),
            )]),
            sections: BTreeMap::from([(
                "vendor.future".into(),
                PreservedSection {
                    descriptor: DomainSectionRecord {
                        domain: "vendor.future".into(),
                        schema_version: 7,
                        revision: 0,
                        payload_key: "vendor.future.bin".into(),
                        encoding: "binary".into(),
                    },
                    bytes: vec![9, 8, 7],
                },
            )]),
        }
    }

    fn seed(package: &TempPackage, project: &DawProject) {
        package
            .actions()
            .save(project, preserved())
            .expect("seed project");
    }

    fn project_with_missing_asset(path: &Path) -> (DawProject, AssetId) {
        let mut project = DawProject::new("missing media", 48_000, 120.0).unwrap();
        let absolute = AbsolutePath::parse(path.to_string_lossy().into_owned()).unwrap();
        let location = AssetLocation::new(Some(absolute), None).unwrap();
        let mut asset = None;
        project
            .transact(
                "register missing media",
                project.revisions().aggregate,
                BTreeSet::from([crate::daw_project::ProjectDomain::Assets]),
                |state| -> Result<(), crate::assets::AssetError> {
                    asset = Some(state.domains.assets.register(AssetRegistration {
                        name: "missing.wav".into(),
                        location: location.clone(),
                        metadata: DecodedAudioMetadata {
                            sample_rate_hz: 48_000,
                            channels: 1,
                            frame_count: SampleFrames(4),
                            container: Some("wav".into()),
                            codec: Some("pcm_f32le".into()),
                            bit_depth: Some(32),
                        },
                        content: ContentFingerprint::from_bytes(b"missing-media"),
                        provenance: AssetProvenance::new(
                            1,
                            AssetOrigin::ImportedFile {
                                importer: "test".into(),
                            },
                            location.clone(),
                        ),
                        tags: BTreeSet::new(),
                        favorite: false,
                    })?);
                    Ok(())
                },
            )
            .unwrap();
        (project, asset.unwrap())
    }

    fn open(package: &TempPackage) -> TestDocument {
        let mut document = TestDocument::new(41);
        let request = document.begin_open_primary(package.actions());
        let completion = request.load(&MissingDecoder);
        document.finish_open(completion, None).unwrap();
        document
    }

    fn add_bus(document: &mut TestDocument, name: &str) {
        let (base_revision, command) = {
            let snapshot = document.session().project_snapshot().unwrap();
            let command = MixerCommand::build(
                format!("add {name}"),
                &snapshot.project.state().domains.mixer,
                |mixer| {
                    mixer.add_bus(BusKind::Source, name)?;
                    Ok(())
                },
            )
            .unwrap();
            (snapshot.revisions().aggregate, command)
        };
        let commands = vec![DomainCommand::Mixer(command)];
        document
            .session_mut()
            .execute(CommandEnvelope {
                label: format!("add {name}"),
                base_revision,
                coalesce: None,
                id_claims: claims_for_commands(&commands),
                commands,
            })
            .unwrap();
    }

    #[test]
    fn open_edit_save_reopen_keeps_workspace_and_unknown_data() {
        let package = TempPackage::new("round-trip");
        seed(
            &package,
            &DawProject::new("round trip", 48_000, 120.0).unwrap(),
        );
        let mut document = open(&package);
        let workspace = WorkspaceDocument::default();
        document.replace_workspace(Some(workspace.clone()));
        add_bus(&mut document, "Drums");
        assert!(document.is_dirty().unwrap());

        let completion = document.begin_save().unwrap().persist();
        let saved = document.finish_save(completion).unwrap();
        assert!(saved.project_marked_saved);
        assert!(saved.workspace_marked_saved);
        assert!(saved.document_clean);
        assert!(!document.is_dirty().unwrap());

        let reopened = open(&package);
        assert_eq!(reopened.workspace(), Some(&workspace));
        assert_eq!(
            reopened.preserved().envelope_extensions["vendor.note"],
            serde_json::Value::String("keep".into())
        );
        assert_eq!(
            reopened.preserved().sections["vendor.future"].bytes,
            vec![9, 8, 7]
        );
        assert!(reopened
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .any(|bus| bus.name() == "Drums"));
    }

    #[test]
    fn recovery_requires_choice_and_installs_selected_checkpoint() {
        let package = TempPackage::new("recovery");
        let primary = DawProject::new("primary", 48_000, 120.0).unwrap();
        seed(&package, &primary);
        let mut recovered = primary.clone();
        recovered
            .transact(
                "recovered edit",
                recovered.revisions().aggregate,
                BTreeSet::from([crate::daw_project::ProjectDomain::Mixer]),
                |state| -> Result<(), crate::mixer::MixerError> {
                    state.domains.mixer.add_bus(BusKind::Source, "Recovered")?;
                    Ok(())
                },
            )
            .unwrap();
        package
            .actions()
            .autosave(&recovered, preserved(), 77)
            .unwrap();
        let checkpoint = package.actions().recovery_options().checkpoints[0].clone();

        let ordinary = open(&package);
        assert_eq!(ordinary.origin(), Some(&ProjectDocumentOrigin::Primary));
        assert!(!ordinary
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .any(|bus| bus.name() == "Recovered"));

        let mut document = TestDocument::new(42);
        let completion = document
            .begin_open_recovery(package.actions(), checkpoint.clone())
            .load(&MissingDecoder);
        let outcome = document.finish_open(completion, None).unwrap();
        assert_eq!(outcome.origin, ProjectDocumentOrigin::Recovery(checkpoint));
        assert!(document
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .any(|bus| bus.name() == "Recovered"));
        assert!(document.is_dirty().unwrap());

        let completion = document.begin_save().unwrap().persist();
        let saved = document.finish_save(completion).unwrap();
        assert!(saved.document_clean);
        assert_eq!(document.origin(), Some(&ProjectDocumentOrigin::Primary));
        assert!(open(&package)
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .any(|bus| bus.name() == "Recovered"));
    }

    #[test]
    fn save_revision_conflict_keeps_newer_project_and_workspace_dirty() {
        let package = TempPackage::new("conflict");
        seed(
            &package,
            &DawProject::new("conflict", 48_000, 120.0).unwrap(),
        );
        let mut document = open(&package);
        add_bus(&mut document, "First");
        let request = document.begin_save().unwrap();
        assert_eq!(request.aggregate_revision(), 1);
        let completion = request.persist();

        add_bus(&mut document, "Second");
        document.replace_workspace(Some(WorkspaceDocument::default()));
        let outcome = document.finish_save(completion).unwrap();
        assert!(!outcome.project_marked_saved);
        assert!(!outcome.workspace_marked_saved);
        assert!(!outcome.document_clean);
        assert!(document.is_dirty().unwrap());

        let reopened = open(&package);
        let names = reopened
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .map(|bus| bus.name())
            .collect::<Vec<_>>();
        assert!(names.contains(&"First"));
        assert!(!names.contains(&"Second"));
        assert!(reopened.workspace().is_none());
    }

    #[test]
    fn save_as_changes_location_and_export_rejects_a_stale_audible_revision() {
        let source = TempPackage::new("save-as-source");
        let destination = TempPackage::new("save-as-destination");
        seed(&source, &DawProject::new("save as", 48_000, 120.0).unwrap());
        let mut document = open(&source);
        add_bus(&mut document, "Moved");
        let completion = document
            .begin_save_as(destination.actions())
            .unwrap()
            .persist();
        let outcome = document.finish_save(completion).unwrap();
        assert!(outcome.document_clean);
        assert_eq!(
            document.manifest_path(),
            Some(destination.path.join(PACKAGE_MANIFEST_NAME).as_path())
        );

        let format = crate::audio::AudioFormat::new(48_000, 1).unwrap();
        let audio = ProjectAudio::new(format, Arc::from([0.0_f32, 0.25, -0.25, 0.0])).unwrap();
        let export = document
            .begin_export(
                1,
                audio.clone(),
                WavExportRequest::new(destination.path.join("current.wav")),
            )
            .unwrap();
        assert_eq!(export.aggregate_revision(), 1);
        let report = export
            .export(&mut crate::export::NoopExportObserver)
            .unwrap();
        assert_eq!(report.aggregate_revision, 1);

        let error = document
            .begin_export(
                0,
                audio,
                WavExportRequest::new(destination.path.join("stale.wav")),
            )
            .err()
            .expect("stale audio must not be exported as current");
        assert!(matches!(
            error,
            ProjectLifecycleError::ExportRevisionConflict {
                project_revision: 1,
                audible_revision: 0
            }
        ));
    }

    #[test]
    fn open_installs_with_typed_missing_media_diagnostics() {
        let package = TempPackage::new("missing-media");
        let missing = package.path.join("does-not-exist.wav");
        let (project, asset) = project_with_missing_asset(&missing);
        seed(&package, &project);

        let document = open(&package);
        assert_eq!(document.diagnostics().unresolved_assets, vec![asset]);
        assert!(document
            .diagnostics()
            .media
            .iter()
            .any(|diagnostic| diagnostic.asset == asset && diagnostic.code == "route-missing"));
        assert!(document
            .session()
            .project_snapshot()
            .unwrap()
            .pcm
            .is_empty());
        assert!(!document.session().diagnostics().is_empty());
    }
}
