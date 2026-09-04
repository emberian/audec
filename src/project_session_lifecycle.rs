//! UI-neutral durable lifecycle for one authoritative [`ProjectSession`].
//!
//! Open, save, recovery, and export are split into capture/worker/completion
//! phases. No worker retains a `LiveProject`, and completing a job only ever
//! installs or marks the one controller owned by the session.

use std::error::Error;
use std::fmt;
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::analysis::Analysis;
use crate::assets::AssetId;
use crate::audio::ProjectAudio;
use crate::command::{AssetCommand, CommandEnvelope, DomainCommand};
use crate::command_journal::{
    decode_runtime_frame, recover_prefix, CommandJournalRecord, RuntimeCommandCodec,
};
use crate::daw_engine::AssetPcmMap;
use crate::daw_project::{DawProject, ProjectRevisions};
use crate::export::{
    export_revision_pinned_audio_to_wav, ExportError, ExportObserver, RevisionPinnedAudio,
    RevisionPinnedWavExportReport, WavExportRequest,
};
use crate::file_actions::ProjectFileActions;
use crate::live_project::{
    LiveProject, LiveProjectError, ProjectJournalCheckpoint, ProjectJournalDelta,
};
use crate::media_resolver::{MediaDecoder, RelinkProposal};
use crate::project_format::PreservedProjectData;
use crate::project_io::ProjectIoDiagnostic;
use crate::project_repository::{
    JsonAirPayloadCodec, MediaHydrationDiagnostic, OpenedProject, ProjectRepositoryError,
};
use crate::project_store::{
    JournalRecoveryCandidate, RecoveryCheckpoint, RecoveryDiscovery, SaveResult, StoreDiagnostic,
    DEFAULT_MAX_ACTIVE_JOURNAL_SEGMENTS,
};
use crate::workspace_document::WorkspaceDocument;

use super::{ProjectSession, ProjectSessionError, WorkspaceRevealTargetIssue};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectJournalDiagnosticKind {
    Encoding,
    Persistence,
    Superseded,
    Recovery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectJournalDiagnostic {
    pub kind: ProjectJournalDiagnosticKind,
    pub message: String,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectJournalPersistenceState {
    NoPending {
        checkpoint: ProjectJournalCheckpoint,
    },
    NotWritten {
        checkpoint: ProjectJournalCheckpoint,
        through_sequence: u64,
        resulting_revision: u64,
    },
    Persisted {
        prior_checkpoint: ProjectJournalCheckpoint,
        checkpoint: ProjectJournalCheckpoint,
        path: PathBuf,
        compaction: ProjectJournalCompactionState,
    },
    DurableSuperseded {
        durable_through_sequence: u64,
        durable_revision: u64,
        current_checkpoint: ProjectJournalCheckpoint,
        path: PathBuf,
    },
    Failed {
        checkpoint: ProjectJournalCheckpoint,
        through_sequence: u64,
        resulting_revision: u64,
        message: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectJournalCompactionState {
    pub compacted_path: Option<PathBuf>,
    pub active_segments: usize,
    pub frames_preserved: usize,
    pub skipped_reason: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectJournalRecoveryState {
    pub discovered_segments: Vec<PathBuf>,
    pub discovered_candidates: Vec<JournalRecoveryCandidate>,
    pub checkpoint: Option<ProjectJournalCheckpoint>,
    pub replayed_records: usize,
    pub replay: ProjectJournalReplayState,
    pub last_persistence: Option<ProjectJournalPersistenceState>,
    pub diagnostics: Vec<ProjectJournalDiagnostic>,
}

/// User-presentable disposition of journal recovery for the installed
/// document. Recovery is opt-in: ordinary open reports discovered segments
/// without interpreting them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProjectJournalReplayState {
    #[default]
    NotRequested,
    Current {
        revision: u64,
        through_sequence: u64,
    },
    Replayed {
        from_revision: u64,
        through_revision: u64,
        through_sequence: u64,
        records: usize,
    },
    Partial {
        from_revision: u64,
        through_revision: u64,
        through_sequence: u64,
        records: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectDocumentOrigin {
    Primary,
    Recovery(RecoveryCheckpoint),
}

/// Whether replacing the document can proceed without an explicit user
/// choice.  This is intentionally shared by New, Open Project, Open Audio,
/// and recovery adapters: all four destroy the same authoritative session
/// state even though only two of them load a project package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectReplacementDisposition {
    /// No project is installed, so there is nothing to lose.
    Empty,
    /// The installed project and durable workspace match their last save.
    Clean,
    /// Project commands, workspace presentation, or recovery provenance have
    /// not been made durable. A host must ask Save / Discard / Cancel.
    Dirty,
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
    pub stale_workspace_targets: Vec<WorkspaceRevealTargetIssue>,
    pub journal: Vec<ProjectJournalDiagnostic>,
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
            .chain(self.stale_workspace_targets.iter().map(ToString::to_string))
            .chain(
                self.journal
                    .iter()
                    .map(|diagnostic| diagnostic.message.clone()),
            )
            .collect()
    }
}

/// Persistence state paired with one application-owned [`ProjectSession`].
/// This is not a second project model: all editable state and history remain
/// in the session passed to each lifecycle boundary.
pub struct ProjectDocumentLifecycle<C = JsonAirPayloadCodec> {
    files: Option<ProjectFileActions>,
    manifest_path: Option<PathBuf>,
    origin: Option<ProjectDocumentOrigin>,
    preserved: PreservedProjectData,
    workspace: Option<WorkspaceDocument>,
    workspace_revision: u64,
    saved_workspace_revision: u64,
    diagnostics: ProjectDocumentDiagnostics,
    recovery: RecoveryDiscovery,
    journal: ProjectJournalRecoveryState,
    document_epoch: u64,
    operation_sequence: u64,
    pending_open: Option<PendingProjectOpen>,
    latest_primary_save: Option<u64>,
    /// Vestigial. `src/ui.rs:1621` still spells this type
    /// `ProjectDocumentLifecycle<JsonAirPayloadCodec>`; the parameter and this
    /// field go with that line.
    codec: PhantomData<C>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingProjectOpen {
    token: u64,
    source: PathBuf,
}

impl<C> ProjectDocumentLifecycle<C> {
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
            journal: ProjectJournalRecoveryState::default(),
            document_epoch: 0,
            operation_sequence: 0,
            pending_open: None,
            latest_primary_save: None,
            codec: PhantomData,
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

    pub fn journal_recovery_state(&self) -> &ProjectJournalRecoveryState {
        &self.journal
    }

    pub fn is_dirty(&self, session: &ProjectSession) -> Result<bool, ProjectLifecycleError> {
        let project_dirty = match session.project_snapshot() {
            Ok(_) => self.files.is_none() || session.is_dirty()?,
            Err(ProjectSessionError::NoProject) => false,
            Err(error) => return Err(ProjectLifecycleError::Session(error)),
        };
        Ok(matches!(
            self.origin.as_ref(),
            Some(ProjectDocumentOrigin::Recovery(_))
        ) || project_dirty
            || self.workspace_revision != self.saved_workspace_revision)
    }

    /// Classify a destructive document replacement without mutating either
    /// the lifecycle or the session. UI adapters should call this before New,
    /// Open Project, Open Audio, or recovery. `Empty` is not reported as an
    /// error because opening is the operation which creates the first project.
    pub fn replacement_disposition(
        &self,
        session: &ProjectSession,
    ) -> Result<ProjectReplacementDisposition, ProjectLifecycleError> {
        match session.project_snapshot() {
            Err(ProjectSessionError::NoProject) => Ok(ProjectReplacementDisposition::Empty),
            Err(error) => Err(ProjectLifecycleError::Session(error)),
            Ok(_) if self.is_dirty(session)? => Ok(ProjectReplacementDisposition::Dirty),
            Ok(_) => Ok(ProjectReplacementDisposition::Clean),
        }
    }

    /// Refuse a destructive replacement when Save / Discard / Cancel has not
    /// yet been resolved by the host.
    pub fn ensure_replaceable(
        &self,
        session: &ProjectSession,
    ) -> Result<ProjectReplacementDisposition, ProjectLifecycleError> {
        let disposition = self.replacement_disposition(session)?;
        if disposition == ProjectReplacementDisposition::Dirty {
            Err(ProjectLifecycleError::UnsavedChanges)
        } else {
            Ok(disposition)
        }
    }

    /// Source currently loading in the background. The installed document
    /// remains usable until a matching completion has been validated.
    pub fn pending_open_source(&self) -> Option<&Path> {
        self.pending_open
            .as_ref()
            .map(|pending| pending.source.as_path())
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
        files: ProjectFileActions,
    ) -> Result<ProjectOpenRequest, ProjectLifecycleError> {
        self.ensure_replaceable(session)?;
        Ok(self.begin_open(files, ProjectDocumentOrigin::Primary))
    }

    /// Recovery is impossible without passing a checkpoint chosen by the UI.
    pub fn begin_open_recovery(
        &mut self,
        session: &mut ProjectSession,
        files: ProjectFileActions,
        checkpoint: RecoveryCheckpoint,
    ) -> Result<ProjectOpenRequest, ProjectLifecycleError> {
        self.ensure_replaceable(session)?;
        Ok(self.begin_open(files, ProjectDocumentOrigin::Recovery(checkpoint)))
    }

    /// Begin opening after the host has received an explicit Discard choice.
    /// The irreversible intent is visible in the method name so a caller
    /// cannot accidentally bypass dirty-state policy while handling a path.
    pub fn begin_open_primary_discarding_changes(
        &mut self,
        files: ProjectFileActions,
    ) -> ProjectOpenRequest {
        self.begin_open(files, ProjectDocumentOrigin::Primary)
    }

    /// Recovery counterpart to
    /// [`begin_open_primary_discarding_changes`](Self::begin_open_primary_discarding_changes).
    pub fn begin_open_recovery_discarding_changes(
        &mut self,
        files: ProjectFileActions,
        checkpoint: RecoveryCheckpoint,
    ) -> ProjectOpenRequest {
        self.begin_open(files, ProjectDocumentOrigin::Recovery(checkpoint))
    }

    fn begin_open(
        &mut self,
        files: ProjectFileActions,
        origin: ProjectDocumentOrigin,
    ) -> ProjectOpenRequest {
        let token = self.next_operation();
        let source = match &origin {
            ProjectDocumentOrigin::Primary => files.repository().store().package().manifest_path(),
            ProjectDocumentOrigin::Recovery(checkpoint) => checkpoint.manifest_path.clone(),
        };
        // Loading is transactional: keep the installed document, repository,
        // workspace, and dirty state usable until a matching completion has
        // decoded, hydrated, and constructed a valid LiveProject. A failed
        // Open must never strand the user in a Failed session which can no
        // longer save the project they had before choosing the path.
        self.pending_open = Some(PendingProjectOpen { token, source });
        ProjectOpenRequest {
            token,
            files,
            origin,
        }
    }

    pub fn finish_open(
        &mut self,
        session: &mut ProjectSession,
        completion: ProjectOpenCompletion,
        analysis: Option<Arc<Analysis>>,
    ) -> Result<ProjectOpenOutcome, ProjectLifecycleError> {
        if self.pending_open.as_ref().map(|pending| pending.token) != Some(completion.token) {
            return Err(ProjectLifecycleError::StaleOpenCompletion);
        }
        self.pending_open = None;
        let loaded = match completion.loaded {
            Ok(loaded) => loaded,
            Err(error) => return Err(ProjectLifecycleError::Repository(error)),
        };
        let LoadedDocument {
            opened,
            hydration,
            recovery,
            journal: prepared_journal,
        } = loaded;
        let OpenedProject {
            project,
            workspace,
            preserved,
            diagnostics: project_io,
            manifest_path,
        } = opened;
        let live = match LiveProject::from_project(project, hydration.pcm) {
            Ok(live) => live,
            Err(error) => return Err(ProjectLifecycleError::LiveProject(error)),
        };
        let mut revisions = match session.install(live, analysis) {
            Ok(revisions) => revisions,
            Err(error) => return Err(ProjectLifecycleError::Session(error)),
        };
        let mut journal_diagnostics = Vec::new();
        let mut replayed_records = 0;
        let mut replay_state = ProjectJournalReplayState::NotRequested;
        if let Some(prepared) = prepared_journal {
            let replay_requested = prepared.requested;
            journal_diagnostics = prepared.diagnostics;
            let requested_records = prepared.records.len();
            let first_sequence = prepared
                .records
                .first()
                .map(|prepared| prepared.record.sequence);
            let next_sequence = prepared
                .anchor_sequence
                .and_then(|sequence| sequence.checked_add(1))
                .or(first_sequence);
            if let Some(next_sequence) = next_sequence {
                if let Err(error) = session.begin_journal_replay(next_sequence) {
                    journal_diagnostics.push(ProjectJournalDiagnostic {
                        kind: ProjectJournalDiagnosticKind::Recovery,
                        message: format!("seeding project journal replay failed: {error}"),
                        path: None,
                    });
                } else {
                    for prepared in prepared.records {
                        match session
                            .replay_record_with_asset_pcm(&prepared.record, prepared.asset_pcm)
                        {
                            Ok(receipt) => {
                                replayed_records += 1;
                                revisions = receipt.publication.snapshot.revisions();
                            }
                            Err(error) => {
                                journal_diagnostics.push(ProjectJournalDiagnostic {
                                    kind: ProjectJournalDiagnosticKind::Recovery,
                                    message: format!(
                                        "replaying project journal record {} failed: {error}",
                                        prepared.record.sequence
                                    ),
                                    path: None,
                                });
                                break;
                            }
                        }
                    }
                }
            }
            let checkpoint = session.journal_checkpoint()?;
            replay_state = if !replay_requested {
                ProjectJournalReplayState::NotRequested
            } else if replayed_records == 0 && journal_diagnostics.is_empty() {
                ProjectJournalReplayState::Current {
                    revision: checkpoint.project_revision,
                    through_sequence: checkpoint.through_sequence,
                }
            } else if replayed_records == requested_records && journal_diagnostics.is_empty() {
                ProjectJournalReplayState::Replayed {
                    from_revision: prepared.from_revision,
                    through_revision: checkpoint.project_revision,
                    through_sequence: checkpoint.through_sequence,
                    records: replayed_records,
                }
            } else {
                ProjectJournalReplayState::Partial {
                    from_revision: prepared.from_revision,
                    through_revision: checkpoint.project_revision,
                    through_sequence: checkpoint.through_sequence,
                    records: replayed_records,
                }
            };
        }
        let diagnostics = ProjectDocumentDiagnostics {
            project_io,
            media: hydration.diagnostics,
            unresolved_assets: hydration.unresolved_assets,
            relink_proposals: hydration.relink_proposals,
            recovery_store: recovery.diagnostics.clone(),
            stale_workspace_targets: workspace
                .as_ref()
                .map(|workspace| session.validate_workspace_reveal_targets(workspace))
                .unwrap_or_default(),
            journal: journal_diagnostics.clone(),
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
        self.recovery = recovery;
        self.journal = ProjectJournalRecoveryState {
            discovered_segments: self.recovery.journals.clone(),
            discovered_candidates: self.recovery.journal_candidates.clone(),
            checkpoint: session.journal_checkpoint().ok(),
            replayed_records,
            replay: replay_state,
            last_persistence: None,
            diagnostics: journal_diagnostics,
        };
        self.latest_primary_save = None;
        self.advance_document_epoch();
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
    ) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
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
        files: ProjectFileActions,
    ) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
        self.begin_primary_save(session, files)
    }

    fn begin_primary_save(
        &mut self,
        session: &ProjectSession,
        files: ProjectFileActions,
    ) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
        let token = self.next_operation();
        self.latest_primary_save = Some(token);
        self.capture_save(session, files, token, SaveKind::Primary)
    }

    pub fn begin_autosave(
        &mut self,
        session: &ProjectSession,
        saved_unix_ms: u64,
    ) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
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
        files: ProjectFileActions,
        token: u64,
        kind: SaveKind,
    ) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
        let snapshot = session.project_snapshot()?;
        // Primary and autosave checkpoints both capture pending provenance.
        // Callers may choose `persist()` (checkpoint only) or
        // `persist_with_journal()` (checkpoint plus durable command suffix),
        // but an explicit save never silently advances an unwritten cursor.
        let journal_delta = session.capture_autosave_journal()?;
        let journal_checkpoint = session.journal_checkpoint()?;
        Ok(ProjectSaveRequest {
            token,
            document_epoch: self.document_epoch,
            files,
            kind,
            project: Arc::clone(&snapshot.project),
            pcm: Arc::clone(&snapshot.pcm),
            workspace: self.workspace.clone(),
            workspace_revision: self.workspace_revision,
            preserved: self.preserved.clone(),
            journal_checkpoint,
            journal_delta,
        })
    }

    pub fn finish_save(
        &mut self,
        session: &mut ProjectSession,
        completion: ProjectSaveCompletion,
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
        let mut journal = completion.journal;
        if let (
            ProjectJournalPersistenceState::Persisted {
                prior_checkpoint,
                checkpoint,
                path,
                ..
            },
            Some(delta),
        ) = (&journal, completion.journal_delta.as_ref())
        {
            if let Err(error) = session.acknowledge_autosave_journal(delta) {
                let current_checkpoint = session.journal_checkpoint()?;
                journal = if current_checkpoint != *prior_checkpoint {
                    ProjectJournalPersistenceState::DurableSuperseded {
                        durable_through_sequence: checkpoint.through_sequence,
                        durable_revision: checkpoint.project_revision,
                        current_checkpoint,
                        path: path.clone(),
                    }
                } else {
                    ProjectJournalPersistenceState::Failed {
                        checkpoint: *prior_checkpoint,
                        through_sequence: checkpoint.through_sequence,
                        resulting_revision: checkpoint.project_revision,
                        message: format!("acknowledging durable project journal failed: {error}"),
                    }
                };
            }
        }
        match completion.kind {
            SaveKind::Autosave { .. } => {
                let recovery = completion.files.recovery_options();
                self.diagnostics.recovery_store = recovery.diagnostics.clone();
                self.diagnostics
                    .recovery_store
                    .extend(result.maintenance_diagnostics.clone());
                self.recovery = recovery;
                self.update_journal_state(session, journal.clone());
                session.replace_diagnostics(self.diagnostics.summaries());
                Ok(ProjectSaveOutcome {
                    result,
                    project_marked_saved: false,
                    workspace_marked_saved: false,
                    document_clean: self.is_dirty(session).map(|dirty| !dirty)?,
                    journal,
                })
            }
            SaveKind::Primary => {
                let repository_changed = self.manifest_path.as_ref() != Some(&result.manifest_path);
                self.latest_primary_save = None;
                let recovery = completion.files.recovery_options();
                self.files = Some(completion.files);
                self.manifest_path = Some(result.manifest_path.clone());
                self.origin = Some(ProjectDocumentOrigin::Primary);
                self.diagnostics.recovery_store = recovery.diagnostics.clone();
                self.diagnostics
                    .recovery_store
                    .extend(result.maintenance_diagnostics.clone());
                self.recovery = recovery;
                session.replace_diagnostics(self.diagnostics.summaries());
                let project_marked_saved =
                    session.mark_saved_if_revision(result.revision_guard.revision)?;
                let workspace_marked_saved =
                    completion.workspace_revision == self.workspace_revision;
                if workspace_marked_saved {
                    self.saved_workspace_revision = completion.workspace_revision;
                }
                self.update_journal_state(session, journal.clone());
                // Save As changes the namespace in which recovery manifests
                // and journal suffixes are durable. An autosave captured for
                // the previous package must not later acknowledge its old
                // journal as though it belonged to this document location.
                if repository_changed {
                    self.advance_document_epoch();
                }
                Ok(ProjectSaveOutcome {
                    result,
                    project_marked_saved,
                    workspace_marked_saved,
                    document_clean: !self.is_dirty(session)?,
                    journal,
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
    ) -> Result<ProjectExportRequest, ProjectLifecycleError> {
        let project_revision = session.project_snapshot()?.revisions().aggregate;
        if audible_revision != project_revision {
            return Err(ProjectLifecycleError::ExportRevisionConflict {
                project_revision,
                audible_revision,
            });
        }
        Ok(ProjectExportRequest {
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

    fn advance_document_epoch(&mut self) {
        self.document_epoch = self.document_epoch.wrapping_add(1);
        if self.document_epoch == 0 {
            self.document_epoch = 1;
        }
    }

    fn update_journal_state(
        &mut self,
        session: &ProjectSession,
        persistence: ProjectJournalPersistenceState,
    ) {
        self.journal.discovered_segments = self.recovery.journals.clone();
        self.journal.discovered_candidates = self.recovery.journal_candidates.clone();
        self.journal.checkpoint = session.journal_checkpoint().ok();
        self.journal.last_persistence = Some(persistence.clone());
        self.journal.diagnostics.clear();
        let diagnostic = match &persistence {
            ProjectJournalPersistenceState::NotWritten { .. } => Some(ProjectJournalDiagnostic {
                kind: ProjectJournalDiagnosticKind::Persistence,
                message: "project checkpoint is durable, but pending command provenance was not written to the journal".into(),
                path: None,
            }),
            ProjectJournalPersistenceState::Failed { message, .. } => {
                Some(ProjectJournalDiagnostic {
                    kind: ProjectJournalDiagnosticKind::Persistence,
                    message: message.clone(),
                    path: None,
                })
            }
            ProjectJournalPersistenceState::DurableSuperseded { path, .. } => {
                Some(ProjectJournalDiagnostic {
                    kind: ProjectJournalDiagnosticKind::Superseded,
                    message: "an older journal segment completed after a newer durable cursor; it was retained but did not move the cursor backward".into(),
                    path: Some(path.clone()),
                })
            }
            ProjectJournalPersistenceState::Persisted { compaction, .. }
                if compaction.error.is_some() => Some(ProjectJournalDiagnostic {
                    kind: ProjectJournalDiagnosticKind::Persistence,
                    message: format!(
                        "journal segment is durable, but compaction failed: {}",
                        compaction.error.as_deref().unwrap_or("unknown error")
                    ),
                    path: None,
                }),
            ProjectJournalPersistenceState::NoPending { .. }
            | ProjectJournalPersistenceState::Persisted { .. } => None,
        };
        if let Some(diagnostic) = diagnostic {
            self.journal.diagnostics.push(diagnostic.clone());
            self.diagnostics.journal = vec![diagnostic];
        } else {
            self.diagnostics.journal.clear();
        }
    }
}

impl<C> Default for ProjectDocumentLifecycle<C> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveKind {
    Primary,
    Autosave { saved_unix_ms: u64 },
}

pub struct ProjectOpenRequest {
    token: u64,
    files: ProjectFileActions,
    origin: ProjectDocumentOrigin,
}

impl ProjectOpenRequest {
    pub fn load(self, decoder: &impl MediaDecoder) -> ProjectOpenCompletion {
        let opened = match &self.origin {
            ProjectDocumentOrigin::Primary => self.files.open(),
            ProjectDocumentOrigin::Recovery(checkpoint) => self.files.open_recovery(checkpoint),
        };
        let loaded = opened.map(|opened| {
            let hydration = self.files.hydrate(&opened.project, decoder);
            let recovery = self.files.recovery_options();
            let journal =
                discover_journal_cursor(opened.project.revisions().aggregate, &recovery.journals);
            LoadedDocument {
                opened,
                hydration,
                recovery,
                journal: Some(journal),
            }
        });
        ProjectOpenCompletion {
            token: self.token,
            files: self.files,
            origin: self.origin,
            loaded,
        }
    }

    /// Load the chosen durable snapshot and prepare every verified journal
    /// record that extends it. Decoding, command preflight, and media
    /// rematerialization happen off the session; `finish_open` remains the
    /// sole point which mutates and publishes project truth.
    pub fn load_with_journal<J>(
        self,
        decoder: &impl MediaDecoder,
        codec: &J,
    ) -> ProjectOpenCompletion
    where
        J: RuntimeCommandCodec,
    {
        let opened = match &self.origin {
            ProjectDocumentOrigin::Primary => self.files.open(),
            ProjectDocumentOrigin::Recovery(checkpoint) => self.files.open_recovery(checkpoint),
        };
        let loaded = opened.map(|opened| {
            let hydration = self.files.hydrate(&opened.project, decoder);
            let recovery = self.files.recovery_options();
            let journal = prepare_journal_recovery(
                &self.files,
                &opened.project,
                &recovery.journals,
                decoder,
                codec,
            );
            LoadedDocument {
                opened,
                hydration,
                recovery,
                journal: Some(journal),
            }
        });
        ProjectOpenCompletion {
            token: self.token,
            files: self.files,
            origin: self.origin,
            loaded,
        }
    }

    /// Open using a decoder whose project-rate policy is derived from the
    /// decoded aggregate, then prepare verified command-journal replay with
    /// that exact same decoder. This is the application path for media
    /// resolvers which cannot know the destination sample rate before reading
    /// the project manifest.
    pub fn load_with_journal_decoder_factory<J, D, F>(
        self,
        codec: &J,
        make_decoder: F,
    ) -> ProjectOpenCompletion
    where
        J: RuntimeCommandCodec,
        D: MediaDecoder,
        F: FnOnce(&DawProject) -> D,
    {
        let opened = match &self.origin {
            ProjectDocumentOrigin::Primary => self.files.open(),
            ProjectDocumentOrigin::Recovery(checkpoint) => self.files.open_recovery(checkpoint),
        };
        let loaded = opened.map(|opened| {
            let decoder = make_decoder(&opened.project);
            let hydration = self.files.hydrate(&opened.project, &decoder);
            let recovery = self.files.recovery_options();
            let journal = prepare_journal_recovery(
                &self.files,
                &opened.project,
                &recovery.journals,
                &decoder,
                codec,
            );
            LoadedDocument {
                opened,
                hydration,
                recovery,
                journal: Some(journal),
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

pub struct ProjectOpenCompletion {
    token: u64,
    files: ProjectFileActions,
    origin: ProjectDocumentOrigin,
    loaded: Result<LoadedDocument, ProjectRepositoryError>,
}

struct LoadedDocument {
    opened: OpenedProject,
    hydration: crate::project_repository::MediaHydration,
    recovery: RecoveryDiscovery,
    journal: Option<PreparedJournalRecovery>,
}

#[derive(Clone, Debug)]
struct PreparedJournalRecord {
    record: CommandJournalRecord,
    asset_pcm: AssetPcmMap,
}

#[derive(Clone, Debug)]
struct PreparedJournalRecovery {
    requested: bool,
    from_revision: u64,
    anchor_sequence: Option<u64>,
    records: Vec<PreparedJournalRecord>,
    diagnostics: Vec<ProjectJournalDiagnostic>,
}

fn discover_journal_cursor(revision: u64, paths: &[PathBuf]) -> PreparedJournalRecovery {
    let mut anchor_sequence = None;
    let mut diagnostics = Vec::new();
    for path in paths {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                diagnostics.push(ProjectJournalDiagnostic {
                    kind: ProjectJournalDiagnosticKind::Recovery,
                    message: format!("reading project journal failed: {error}"),
                    path: Some(path.clone()),
                });
                continue;
            }
        };
        let recovered = recover_prefix(&bytes);
        if !recovered.is_complete() {
            diagnostics.push(ProjectJournalDiagnostic {
                kind: ProjectJournalDiagnosticKind::Recovery,
                message: format!(
                    "project journal has a rejected tail after {} verified byte(s): {:?}",
                    recovered.valid_bytes, recovered.tail
                ),
                path: Some(path.clone()),
            });
        }
        for frame in recovered.frames {
            if frame.resulting_revision == revision {
                anchor_sequence = Some(
                    anchor_sequence
                        .map_or(frame.sequence, |current: u64| current.max(frame.sequence)),
                );
            }
        }
    }
    PreparedJournalRecovery {
        requested: false,
        from_revision: revision,
        anchor_sequence,
        records: Vec::new(),
        diagnostics,
    }
}

fn prepare_journal_recovery<J>(
    files: &ProjectFileActions,
    project: &DawProject,
    paths: &[PathBuf],
    decoder: &impl MediaDecoder,
    codec: &J,
) -> PreparedJournalRecovery
where
    J: RuntimeCommandCodec,
{
    let from_revision = project.revisions().aggregate;
    let mut diagnostics = Vec::new();
    let mut records = std::collections::BTreeMap::<u64, (CommandJournalRecord, PathBuf)>::new();
    let mut conflicting_sequences = std::collections::BTreeSet::new();

    for path in paths {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                diagnostics.push(ProjectJournalDiagnostic {
                    kind: ProjectJournalDiagnosticKind::Recovery,
                    message: format!("reading project journal failed: {error}"),
                    path: Some(path.clone()),
                });
                continue;
            }
        };
        let recovered = recover_prefix(&bytes);
        if !recovered.is_complete() {
            diagnostics.push(ProjectJournalDiagnostic {
                kind: ProjectJournalDiagnosticKind::Recovery,
                message: format!(
                    "project journal has a rejected tail after {} verified byte(s): {:?}",
                    recovered.valid_bytes, recovered.tail
                ),
                path: Some(path.clone()),
            });
        }
        for frame in recovered.frames {
            let record = match decode_runtime_frame(&frame, codec) {
                Ok(record) => record,
                Err(error) => {
                    diagnostics.push(ProjectJournalDiagnostic {
                        kind: ProjectJournalDiagnosticKind::Encoding,
                        message: format!("decoding project journal record failed: {error}"),
                        path: Some(path.clone()),
                    });
                    continue;
                }
            };
            if conflicting_sequences.contains(&record.sequence) {
                continue;
            }
            if let Some((previous, previous_path)) = records.get(&record.sequence) {
                if previous != &record {
                    let sequence = record.sequence;
                    let previous_path = previous_path.clone();
                    records.remove(&sequence);
                    conflicting_sequences.insert(sequence);
                    diagnostics.push(ProjectJournalDiagnostic {
                        kind: ProjectJournalDiagnosticKind::Recovery,
                        message: format!(
                            "journal sequence {sequence} conflicts with {}",
                            previous_path.display()
                        ),
                        path: Some(path.clone()),
                    });
                }
            } else {
                records.insert(record.sequence, (record, path.clone()));
            }
        }
    }

    // An opened recovery snapshot may already contain a prefix represented by
    // the journal. Recover its exact durable cursor before looking for the
    // next connected suffix.
    let anchor_sequence = records
        .values()
        .filter(|(record, _)| record.resulting_revision == from_revision)
        .map(|(record, _)| record.sequence)
        .max();
    let mut expected_sequence = anchor_sequence.and_then(|sequence| sequence.checked_add(1));
    let mut revision = from_revision;
    let mut staged = project.clone();
    let mut prepared = Vec::new();

    loop {
        let candidate = match expected_sequence {
            Some(sequence) => records
                .get(&sequence)
                .filter(|(record, _)| record.base_revision == revision),
            None => records
                .values()
                .filter(|(record, _)| record.base_revision == revision)
                .min_by_key(|(record, _)| record.sequence),
        };
        let Some((record, path)) = candidate.cloned() else {
            break;
        };
        let replay_envelope =
            CommandEnvelope::from_batch(record.base_revision, record.batch.clone())
                .rebase_ephemeral_guards_for_replay(&staged);
        let replay_result = replay_envelope.and_then(|envelope| envelope.apply(&mut staged));
        if let Err(error) = replay_result {
            diagnostics.push(ProjectJournalDiagnostic {
                kind: ProjectJournalDiagnosticKind::Recovery,
                message: format!(
                    "journal record {} does not apply to the selected checkpoint: {error}",
                    record.sequence
                ),
                path: Some(path),
            });
            break;
        }

        let present_assets = record
            .batch
            .commands
            .iter()
            .filter_map(|command| match command {
                DomainCommand::Assets(AssetCommand::PutAsset {
                    id,
                    after: Some(asset),
                    ..
                }) if matches!(
                    asset.availability(),
                    crate::assets::AssetAvailability::Present
                ) =>
                {
                    Some(*id)
                }
                DomainCommand::Assets(AssetCommand::PutAvailability { asset, after, .. })
                    if matches!(after, crate::assets::AssetAvailability::Present) =>
                {
                    Some(*asset)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut asset_pcm = AssetPcmMap::new();
        if !present_assets.is_empty() {
            let hydration = files.hydrate(&staged, decoder);
            for asset in present_assets {
                let Some(pcm) = hydration.pcm.get(&asset).cloned() else {
                    diagnostics.push(ProjectJournalDiagnostic {
                        kind: ProjectJournalDiagnosticKind::Recovery,
                        message: format!(
                            "journal record {} creates present asset {} but its exact PCM could not be rematerialized",
                            record.sequence, asset.0
                        ),
                        path: Some(path.clone()),
                    });
                    return PreparedJournalRecovery {
                        requested: true,
                        from_revision,
                        anchor_sequence,
                        records: prepared,
                        diagnostics,
                    };
                };
                asset_pcm.insert(asset, pcm);
            }
        }
        revision = record.resulting_revision;
        expected_sequence = record.sequence.checked_add(1);
        prepared.push(PreparedJournalRecord { record, asset_pcm });
    }

    PreparedJournalRecovery {
        requested: true,
        from_revision,
        anchor_sequence,
        records: prepared,
        diagnostics,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectOpenOutcome {
    pub revisions: ProjectRevisions,
    pub origin: ProjectDocumentOrigin,
    pub manifest_path: PathBuf,
    pub recovery_available: usize,
}

pub struct ProjectSaveRequest {
    token: u64,
    document_epoch: u64,
    files: ProjectFileActions,
    kind: SaveKind,
    project: Arc<DawProject>,
    pcm: Arc<AssetPcmMap>,
    workspace: Option<WorkspaceDocument>,
    workspace_revision: u64,
    preserved: PreservedProjectData,
    journal_checkpoint: ProjectJournalCheckpoint,
    journal_delta: Option<ProjectJournalDelta>,
}

impl ProjectSaveRequest {
    pub fn aggregate_revision(&self) -> u64 {
        self.project.revisions().aggregate
    }

    pub const fn journal_checkpoint(&self) -> ProjectJournalCheckpoint {
        self.journal_checkpoint
    }

    pub fn journal_delta(&self) -> Option<&ProjectJournalDelta> {
        self.journal_delta.as_ref()
    }

    pub fn persist(self) -> ProjectSaveCompletion {
        let result = match self.kind {
            SaveKind::Primary => self.files.save_with_workspace_and_media(
                self.project.as_ref(),
                self.workspace.as_ref(),
                self.preserved,
                self.pcm.as_ref(),
            ),
            SaveKind::Autosave { saved_unix_ms } => self.files.autosave_with_workspace_and_media(
                self.project.as_ref(),
                self.workspace.as_ref(),
                self.preserved,
                saved_unix_ms,
                self.pcm.as_ref(),
            ),
        };
        let journal = self.journal_delta.as_ref().map_or(
            ProjectJournalPersistenceState::NoPending {
                checkpoint: self.journal_checkpoint,
            },
            |delta| ProjectJournalPersistenceState::NotWritten {
                checkpoint: delta.checkpoint,
                through_sequence: delta.through_sequence,
                resulting_revision: delta.resulting_revision,
            },
        );
        ProjectSaveCompletion {
            token: self.token,
            document_epoch: self.document_epoch,
            files: self.files,
            kind: self.kind,
            workspace_revision: self.workspace_revision,
            journal_delta: self.journal_delta,
            journal,
            result,
        }
    }

    /// Persist the recovery snapshot and, if commands are pending, its exact
    /// framed journal suffix. The journal cursor is not advanced here; the UI
    /// thread acknowledges it in `finish_save` after checking document epoch.
    pub fn persist_with_journal<J>(self, codec: &J) -> ProjectSaveCompletion
    where
        J: RuntimeCommandCodec,
    {
        let encoded = self.journal_delta.as_ref().map(|delta| delta.encode(codec));
        let result = match self.kind {
            SaveKind::Primary => self.files.save_with_workspace_and_media(
                self.project.as_ref(),
                self.workspace.as_ref(),
                self.preserved,
                self.pcm.as_ref(),
            ),
            SaveKind::Autosave { saved_unix_ms } => self.files.autosave_with_workspace_and_media(
                self.project.as_ref(),
                self.workspace.as_ref(),
                self.preserved,
                saved_unix_ms,
                self.pcm.as_ref(),
            ),
        };
        let journal = match (&self.journal_delta, encoded) {
            (None, _) => ProjectJournalPersistenceState::NoPending {
                checkpoint: self.journal_checkpoint,
            },
            (Some(delta), Some(Err(error))) => ProjectJournalPersistenceState::Failed {
                checkpoint: delta.checkpoint,
                through_sequence: delta.through_sequence,
                resulting_revision: delta.resulting_revision,
                message: format!("encoding project journal failed: {error}"),
            },
            (Some(delta), Some(Ok(bytes))) if result.is_ok() => {
                let name = journal_segment_name(delta);
                match self.files.write_journal_segment(&name, &bytes) {
                    Ok(path) => ProjectJournalPersistenceState::Persisted {
                        prior_checkpoint: delta.checkpoint,
                        checkpoint: ProjectJournalCheckpoint {
                            through_sequence: delta.through_sequence,
                            project_revision: delta.resulting_revision,
                        },
                        path,
                        compaction: match self
                            .files
                            .compact_journal_segments(DEFAULT_MAX_ACTIVE_JOURNAL_SEGMENTS)
                        {
                            Ok(result) => ProjectJournalCompactionState {
                                compacted_path: result.compacted_path,
                                active_segments: result.active_segments.len(),
                                frames_preserved: result.frames_preserved,
                                skipped_reason: result.skipped_reason,
                                error: None,
                            },
                            Err(error) => ProjectJournalCompactionState {
                                error: Some(error.to_string()),
                                ..ProjectJournalCompactionState::default()
                            },
                        },
                    },
                    Err(error) => ProjectJournalPersistenceState::Failed {
                        checkpoint: delta.checkpoint,
                        through_sequence: delta.through_sequence,
                        resulting_revision: delta.resulting_revision,
                        message: format!("persisting project journal failed: {error}"),
                    },
                }
            }
            (Some(delta), Some(Ok(_))) => ProjectJournalPersistenceState::Failed {
                checkpoint: delta.checkpoint,
                through_sequence: delta.through_sequence,
                resulting_revision: delta.resulting_revision,
                message: "recovery checkpoint failed before its journal could be persisted".into(),
            },
            (Some(_), None) => unreachable!("journal delta and encoding result agree"),
        };
        ProjectSaveCompletion {
            token: self.token,
            document_epoch: self.document_epoch,
            files: self.files,
            kind: self.kind,
            workspace_revision: self.workspace_revision,
            journal_delta: self.journal_delta,
            journal,
            result,
        }
    }
}

fn journal_segment_name(delta: &ProjectJournalDelta) -> String {
    format!(
        "commands-r{:020}-r{:020}-s{:020}-s{:020}.audecj",
        delta.checkpoint.project_revision,
        delta.resulting_revision,
        delta.records.first().map_or(0, |record| record.sequence),
        delta.through_sequence
    )
}

pub struct ProjectSaveCompletion {
    token: u64,
    document_epoch: u64,
    files: ProjectFileActions,
    kind: SaveKind,
    workspace_revision: u64,
    journal_delta: Option<ProjectJournalDelta>,
    journal: ProjectJournalPersistenceState,
    result: Result<SaveResult, ProjectRepositoryError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSaveOutcome {
    pub result: SaveResult,
    pub project_marked_saved: bool,
    pub workspace_marked_saved: bool,
    pub document_clean: bool,
    pub journal: ProjectJournalPersistenceState,
}

pub struct ProjectExportRequest {
    pinned: RevisionPinnedAudio,
    request: WavExportRequest,
}

impl ProjectExportRequest {
    pub fn aggregate_revision(&self) -> u64 {
        self.pinned.aggregate_revision
    }

    pub fn export<O: ExportObserver>(
        self,
        observer: &mut O,
    ) -> Result<RevisionPinnedWavExportReport, ProjectLifecycleError> {
        export_revision_pinned_audio_to_wav(self.pinned, &self.request, observer)
            .map_err(ProjectLifecycleError::Export)
    }
}

#[derive(Debug)]
pub enum ProjectLifecycleError {
    Session(ProjectSessionError),
    Repository(ProjectRepositoryError),
    Export(ExportError),
    LiveProject(LiveProjectError),
    NoRepository,
    StaleOpenCompletion,
    SupersededSaveCompletion,
    DocumentChangedDuringOperation,
    UnsavedChanges,
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
            Self::Export(error) => error.fmt(formatter),
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
            Self::UnsavedChanges => formatter.write_str(
                "the current project has unsaved changes; resolve Save, Discard, or Cancel before replacing it",
            ),
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
            Self::Export(error) => Some(error),
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
    use std::convert::Infallible;
    use std::fs;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::command::{claims_for_commands, CommandBatch, CommandEnvelope, DomainCommand};
    use crate::command_record::{DurableCommandBatch, OpaqueCommandRecord};
    use crate::daw_render::PcmAsset;
    use crate::media_resolver::{DecodedMaterial, MediaDecodeError};
    use crate::mixer::{BusKind, MixerCommand};
    use crate::pattern_actions::{
        CreatePatternIntent, PatternAction, PatternActionIntent,
        PatternEditorMode as PatternActionEditorMode,
    };
    use crate::project_controller::{PatternWorkflowIntent, WorkbenchSampleIntent};
    use crate::project_format::{PreservedSection, ProjectPackage, PACKAGE_MANIFEST_NAME};
    use crate::project_io::DomainSectionRecord;
    use crate::project_repository::ProjectRepository;
    use crate::project_store::ProjectStore;
    use crate::runtime_command_codec::DeterministicRuntimeCommandCodec;
    use crate::sample_actions::SampleKitDestination;
    use crate::sequencer::{BeatDuration, PPQ};
    use crate::session::{Sample, SampleRange};
    use crate::workspace_document::{
        BeatViewport, EditorTarget, EditorViewState, NewWorkspaceView, PatternEditorMode,
        ViewLinkMembership, WorkspaceItemKind,
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    #[derive(Default)]
    struct TestJournalCodec {
        next: AtomicU64,
        batches: Mutex<BTreeMap<u64, CommandBatch>>,
    }

    impl RuntimeCommandCodec for TestJournalCodec {
        type Error = Infallible;

        fn encode_batch(&self, batch: &CommandBatch) -> Result<DurableCommandBatch, Self::Error> {
            let key = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            self.batches.lock().unwrap().insert(key, batch.clone());
            Ok(DurableCommandBatch::new(
                batch.label.clone(),
                vec![OpaqueCommandRecord {
                    domain: "test.lifecycle".into(),
                    kind: "command-batch".into(),
                    schema_version: 1,
                    payload: serde_json::json!({ "key": key }),
                    extensions: BTreeMap::new(),
                }],
            ))
        }

        fn decode_batch(&self, batch: &DurableCommandBatch) -> Result<CommandBatch, Self::Error> {
            let key = batch.commands[0].payload["key"].as_u64().unwrap();
            Ok(self.batches.lock().unwrap()[&key].clone())
        }
    }

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

        fn actions(&self) -> ProjectFileActions {
            let package = ProjectPackage::new(&self.path).unwrap();
            ProjectFileActions::new(ProjectRepository::new(
                ProjectStore::new(package),
                JsonAirPayloadCodec,
            ))
        }
    }

    impl Drop for TempPackage {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct TestDocument {
        lifecycle: ProjectDocumentLifecycle,
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

        fn begin_open_primary(&mut self, files: ProjectFileActions) -> ProjectOpenRequest {
            self.lifecycle
                .begin_open_primary(&mut self.session, files)
                .unwrap()
        }

        fn begin_open_recovery(
            &mut self,
            files: ProjectFileActions,
            checkpoint: RecoveryCheckpoint,
        ) -> ProjectOpenRequest {
            self.lifecycle
                .begin_open_recovery(&mut self.session, files, checkpoint)
                .unwrap()
        }

        fn finish_open(
            &mut self,
            completion: ProjectOpenCompletion,
            analysis: Option<Arc<Analysis>>,
        ) -> Result<ProjectOpenOutcome, ProjectLifecycleError> {
            self.lifecycle
                .finish_open(&mut self.session, completion, analysis)
        }

        fn begin_save(&mut self) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
            self.lifecycle.begin_save(&self.session)
        }

        fn begin_save_as(
            &mut self,
            files: ProjectFileActions,
        ) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
            self.lifecycle.begin_save_as(&self.session, files)
        }

        fn begin_autosave(
            &mut self,
            saved_unix_ms: u64,
        ) -> Result<ProjectSaveRequest, ProjectLifecycleError> {
            self.lifecycle.begin_autosave(&self.session, saved_unix_ms)
        }

        fn finish_save(
            &mut self,
            completion: ProjectSaveCompletion,
        ) -> Result<ProjectSaveOutcome, ProjectLifecycleError> {
            self.lifecycle.finish_save(&mut self.session, completion)
        }

        fn begin_export(
            &self,
            audible_revision: u64,
            audio: ProjectAudio,
            request: WavExportRequest,
        ) -> Result<ProjectExportRequest, ProjectLifecycleError> {
            self.lifecycle
                .begin_export(&self.session, audible_revision, audio, request)
        }
    }

    impl Deref for TestDocument {
        type Target = ProjectDocumentLifecycle;

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

    #[derive(Clone)]
    struct ExactDecoder {
        metadata: DecodedAudioMetadata,
        fingerprint: ContentFingerprint,
        pcm: PcmAsset,
    }

    impl MediaDecoder for ExactDecoder {
        fn decode(&self, path: &Path) -> Result<DecodedMaterial, MediaDecodeError> {
            Ok(DecodedMaterial {
                path: path.to_path_buf(),
                metadata: self.metadata.clone(),
                fingerprint: self.fingerprint,
                pcm: self.pcm.clone(),
            })
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

    fn create_pattern(document: &mut TestDocument, name: &str) {
        let expected_project_revision = document
            .session()
            .project_snapshot()
            .unwrap()
            .revisions()
            .aggregate;
        document
            .session_mut()
            .execute_pattern_workflow(PatternWorkflowIntent::Action(PatternActionIntent {
                expected_project_revision,
                action: PatternAction::Create(CreatePatternIntent {
                    mode: PatternActionEditorMode::Steps,
                    name: name.into(),
                    length: BeatDuration((PPQ * 4) as u64),
                    step_resolution: BeatDuration((PPQ / 4) as u64),
                    initial_target: None,
                }),
            }))
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
    fn dirty_replacement_requires_an_explicit_discard_and_failed_open_is_transactional() {
        let current = TempPackage::new("transactional-open-current");
        seed(
            &current,
            &DawProject::new("current", 48_000, 120.0).unwrap(),
        );
        let missing = TempPackage::new("transactional-open-missing");
        let mut document = open(&current);
        let workspace = WorkspaceDocument::default();
        document.replace_workspace(Some(workspace.clone()));
        add_bus(&mut document, "Unsaved bus");
        let revision = document
            .session()
            .project_snapshot()
            .unwrap()
            .revisions()
            .aggregate;

        assert_eq!(
            document
                .replacement_disposition(document.session())
                .unwrap(),
            ProjectReplacementDisposition::Dirty
        );
        assert!(matches!(
            document
                .lifecycle
                .begin_open_primary(&mut document.session, missing.actions()),
            Err(ProjectLifecycleError::UnsavedChanges)
        ));
        assert!(document.pending_open_source().is_none());

        let request = document
            .lifecycle
            .begin_open_primary_discarding_changes(missing.actions());
        assert_eq!(
            document.pending_open_source(),
            Some(missing.path.join("project.json").as_path())
        );
        let completion = request.load(&MissingDecoder);
        assert!(matches!(
            document.finish_open(completion, None),
            Err(ProjectLifecycleError::Repository(_))
        ));

        // A bad path is only a failed candidate. The old live project,
        // repository identity, workspace, edit history, and dirty state all
        // survive and can still be saved to the original package.
        assert!(document.pending_open_source().is_none());
        assert_eq!(
            document
                .session()
                .project_snapshot()
                .unwrap()
                .revisions()
                .aggregate,
            revision
        );
        assert!(document
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .any(|bus| bus.name() == "Unsaved bus"));
        assert_eq!(document.workspace(), Some(&workspace));
        assert_eq!(
            document.manifest_path(),
            Some(current.path.join("project.json").as_path())
        );
        assert!(document.is_dirty().unwrap());

        let completion = document.begin_save().unwrap().persist();
        assert!(document.finish_save(completion).unwrap().document_clean);
    }

    #[test]
    fn explicit_discard_replaces_a_dirty_document_only_after_load_succeeds() {
        let current = TempPackage::new("discard-current");
        let replacement = TempPackage::new("discard-replacement");
        seed(
            &current,
            &DawProject::new("current", 48_000, 120.0).unwrap(),
        );
        seed(
            &replacement,
            &DawProject::new("replacement", 48_000, 96.0).unwrap(),
        );
        let mut document = open(&current);
        add_bus(&mut document, "Throw away");

        let completion = document
            .lifecycle
            .begin_open_primary_discarding_changes(replacement.actions())
            .load(&MissingDecoder);
        let outcome = document.finish_open(completion, None).unwrap();

        assert_eq!(outcome.origin, ProjectDocumentOrigin::Primary);
        assert_eq!(
            document.session().project_snapshot().unwrap().project.name,
            "replacement"
        );
        assert!(!document
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .any(|bus| bus.name() == "Throw away"));
        assert_eq!(
            document
                .replacement_disposition(document.session())
                .unwrap(),
            ProjectReplacementDisposition::Clean
        );
    }

    #[test]
    fn installed_project_without_a_repository_is_an_unsaved_document() {
        let mut document = TestDocument::new(43);
        let project = DawProject::new("Untitled", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, AssetPcmMap::new()).unwrap();
        document.session_mut().install(live, None).unwrap();

        assert!(document.is_dirty().unwrap());
        assert_eq!(
            document
                .replacement_disposition(document.session())
                .unwrap(),
            ProjectReplacementDisposition::Dirty
        );
    }

    #[test]
    fn installed_project_without_a_repository_can_export_revision_pinned_audio() {
        let destination = TempPackage::new("unsaved-export");
        fs::create_dir_all(&destination.path).unwrap();
        let mut document = TestDocument::new(44);
        let project = DawProject::new("Untitled", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, AssetPcmMap::new()).unwrap();
        document.session_mut().install(live, None).unwrap();

        let format = crate::audio::AudioFormat::new(48_000, 1).unwrap();
        let audio = ProjectAudio::new(format, Arc::from([0.0_f32, 0.5, -0.5, 0.0])).unwrap();
        let wav_path = destination.path.join("unsaved.wav");
        let export = document
            .begin_export(0, audio, WavExportRequest::new(&wav_path))
            .unwrap();
        let report = export
            .export(&mut crate::export::NoopExportObserver)
            .unwrap();

        assert_eq!(report.aggregate_revision, 0);
        assert_eq!(&fs::read(wav_path).unwrap()[..4], b"RIFF");
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
    fn save_as_invalidates_an_autosave_completion_from_the_old_package() {
        let source = TempPackage::new("save-as-autosave-source");
        let destination = TempPackage::new("save-as-autosave-destination");
        seed(
            &source,
            &DawProject::new("save as autosave", 48_000, 120.0).unwrap(),
        );
        let codec = TestJournalCodec::default();
        let mut document = open(&source);
        add_bus(&mut document, "Before move");

        let old_autosave = document
            .begin_autosave(100)
            .unwrap()
            .persist_with_journal(&codec);
        let save_as = document
            .begin_save_as(destination.actions())
            .unwrap()
            .persist_with_journal(&codec);
        document.finish_save(save_as).unwrap();

        assert!(matches!(
            document.finish_save(old_autosave),
            Err(ProjectLifecycleError::DocumentChangedDuringOperation)
        ));
        assert_eq!(
            document.manifest_path(),
            Some(destination.path.join(PACKAGE_MANIFEST_NAME).as_path())
        );
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

    #[test]
    fn save_reopen_resolves_exact_durable_workspace_target() {
        let package = TempPackage::new("durable-workspace-target");
        let project = DawProject::new("durable target", 48_000, 120.0).unwrap();
        let master = project.state().domains.mixer.master();
        let mut workspace = WorkspaceDocument::default();
        workspace
            .create_view(NewWorkspaceView {
                kind: WorkspaceItemKind::Mixer,
                target: EditorTarget::Mixer {
                    bus_id: Some(master.get()),
                },
                title_override: Some("Master".into()),
                links: ViewLinkMembership::default(),
                state: EditorViewState::Mixer,
                extensions: BTreeMap::new(),
            })
            .unwrap();
        package
            .actions()
            .save_with_workspace(&project, Some(&workspace), preserved())
            .unwrap();

        let reopened = open(&package);
        assert!(reopened.diagnostics().stale_workspace_targets.is_empty());
        assert_eq!(
            reopened
                .session()
                .resolve_workspace_target(&EditorTarget::Mixer {
                    bus_id: Some(master.get()),
                }),
            super::super::reveal::WorkspaceTargetResolution::Object(
                crate::project_controller::ObjectRef::Bus(master)
            )
        );
    }

    #[test]
    fn imported_workspace_reports_stale_project_descriptor() {
        let package = TempPackage::new("stale-workspace-target");
        let project = DawProject::new("stale target", 48_000, 120.0).unwrap();
        let mut workspace = WorkspaceDocument::default();
        let stale_view = workspace
            .create_view(NewWorkspaceView {
                kind: WorkspaceItemKind::PatternEditor {
                    mode: PatternEditorMode::Steps,
                },
                target: EditorTarget::PatternDefinition { id: 999 },
                title_override: Some("Deleted pattern".into()),
                links: ViewLinkMembership::default(),
                state: EditorViewState::Pattern {
                    viewport: BeatViewport {
                        start_tick: 0,
                        end_tick: 3_840,
                    },
                    vertical_origin: None,
                },
                extensions: BTreeMap::new(),
            })
            .unwrap();
        package
            .actions()
            .save_with_workspace(&project, Some(&workspace), preserved())
            .unwrap();

        let reopened = open(&package);
        assert_eq!(reopened.diagnostics().stale_workspace_targets.len(), 1);
        let issue = &reopened.diagnostics().stale_workspace_targets[0];
        assert_eq!(issue.view, stale_view);
        assert_eq!(
            issue.reason,
            super::super::reveal::WorkspaceRevealTargetIssueReason::MissingProjectObject
        );
        assert_eq!(
            issue.object,
            Some(crate::project_controller::ObjectRef::Pattern(
                crate::sequencer::PatternId::from_raw(999)
            ))
        );
        assert!(reopened
            .session()
            .diagnostics()
            .iter()
            .any(|message| message.contains("missing pattern:999")));
    }

    #[test]
    fn raced_autosave_acknowledges_only_its_exact_durable_prefix() {
        let package = TempPackage::new("journal-race");
        seed(
            &package,
            &DawProject::new("journal race", 48_000, 120.0).unwrap(),
        );
        let codec = TestJournalCodec::default();
        let mut document = open(&package);
        add_bus(&mut document, "First");
        let request = document.begin_autosave(10).unwrap();
        let captured = request.journal_delta().unwrap().clone();
        assert_eq!(captured.through_sequence, 1);
        let completion = request.persist_with_journal(&codec);

        add_bus(&mut document, "Second");
        let outcome = document.finish_save(completion).unwrap();
        assert!(matches!(
            outcome.journal,
            ProjectJournalPersistenceState::Persisted { checkpoint, .. }
                if checkpoint.through_sequence == 1 && checkpoint.project_revision == 1
        ));
        let pending = document
            .session()
            .capture_autosave_journal()
            .unwrap()
            .unwrap();
        assert_eq!(pending.checkpoint.through_sequence, 1);
        assert_eq!(pending.records.len(), 1);
        assert_eq!(pending.records[0].sequence, 2);
        assert_eq!(pending.resulting_revision, 2);
    }

    #[test]
    fn older_autosave_completion_never_rewinds_the_durable_cursor() {
        let package = TempPackage::new("journal-out-of-order");
        seed(
            &package,
            &DawProject::new("journal order", 48_000, 120.0).unwrap(),
        );
        let codec = TestJournalCodec::default();
        let mut document = open(&package);
        add_bus(&mut document, "First");
        let older = document
            .begin_autosave(20)
            .unwrap()
            .persist_with_journal(&codec);
        add_bus(&mut document, "Second");
        let newer = document
            .begin_autosave(21)
            .unwrap()
            .persist_with_journal(&codec);

        let newer = document.finish_save(newer).unwrap();
        assert!(matches!(
            newer.journal,
            ProjectJournalPersistenceState::Persisted { checkpoint, .. }
                if checkpoint.through_sequence == 2
        ));
        let older = document.finish_save(older).unwrap();
        assert!(matches!(
            older.journal,
            ProjectJournalPersistenceState::DurableSuperseded {
                durable_through_sequence: 1,
                current_checkpoint,
                ..
            } if current_checkpoint.through_sequence == 2
        ));
        assert_eq!(
            document.session().journal_checkpoint().unwrap(),
            ProjectJournalCheckpoint {
                through_sequence: 2,
                project_revision: 2,
            }
        );
        assert_eq!(
            document.journal_recovery_state().diagnostics[0].kind,
            ProjectJournalDiagnosticKind::Superseded
        );
    }

    #[test]
    fn chosen_checkpoint_replays_connected_journal_suffix_and_restores_cursor() {
        let package = TempPackage::new("journal-replay");
        seed(
            &package,
            &DawProject::new("journal replay", 48_000, 120.0).unwrap(),
        );
        let codec = TestJournalCodec::default();
        let mut writer = open(&package);
        create_pattern(&mut writer, "First");
        let first = writer
            .begin_autosave(30)
            .unwrap()
            .persist_with_journal(&codec);
        writer.finish_save(first).unwrap();
        create_pattern(&mut writer, "Second");
        let second = writer
            .begin_autosave(31)
            .unwrap()
            .persist_with_journal(&codec);
        writer.finish_save(second).unwrap();

        let primary = writer.begin_save().unwrap().persist();
        writer.finish_save(primary).unwrap();
        let reopened = open(&package);
        assert_eq!(
            reopened.session().journal_checkpoint().unwrap(),
            ProjectJournalCheckpoint {
                through_sequence: 2,
                project_revision: 2,
            },
            "ordinary reopen reconstructs the durable cursor without replay"
        );

        let checkpoint = package
            .actions()
            .recovery_options()
            .checkpoints
            .into_iter()
            .find(|checkpoint| checkpoint.base_project_revision == 1)
            .expect("first autosave checkpoint");
        let mut recovered = TestDocument::new(88);
        let completion = recovered
            .begin_open_recovery(package.actions(), checkpoint)
            .load_with_journal(&MissingDecoder, &codec);
        let outcome = recovered.finish_open(completion, None).unwrap();
        assert_eq!(
            outcome.revisions.aggregate,
            2,
            "journal recovery state: {:?}",
            recovered.journal_recovery_state()
        );
        let names = recovered
            .session()
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .sequencer
            .patterns()
            .patterns()
            .map(|pattern| pattern.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"First"));
        assert!(names.contains(&"Second"));
        assert_eq!(
            recovered.session().journal_checkpoint().unwrap(),
            ProjectJournalCheckpoint {
                through_sequence: 2,
                project_revision: 2,
            }
        );
        assert_eq!(recovered.journal_recovery_state().replayed_records, 1);
        assert!(matches!(
            recovered.journal_recovery_state().replay,
            ProjectJournalReplayState::Replayed {
                from_revision: 1,
                through_revision: 2,
                through_sequence: 2,
                records: 1,
            }
        ));
    }

    #[test]
    fn chosen_checkpoint_rebases_ephemeral_mixer_guard_for_journal_replay() {
        let package = TempPackage::new("journal-mixer-replay");
        seed(
            &package,
            &DawProject::new("journal mixer replay", 48_000, 120.0).unwrap(),
        );
        let writer_codec = DeterministicRuntimeCommandCodec;
        let mut writer = open(&package);
        add_bus(&mut writer, "First");
        let first = writer
            .begin_autosave(35)
            .unwrap()
            .persist_with_journal(&writer_codec);
        writer.finish_save(first).unwrap();
        add_bus(&mut writer, "Second");
        let second = writer
            .begin_autosave(36)
            .unwrap()
            .persist_with_journal(&writer_codec);
        writer.finish_save(second).unwrap();

        let checkpoint = package
            .actions()
            .recovery_options()
            .checkpoints
            .into_iter()
            .find(|checkpoint| checkpoint.base_project_revision == 1)
            .expect("first mixer autosave checkpoint");
        let mut recovered = TestDocument::new(90);
        let restarted_codec = DeterministicRuntimeCommandCodec;
        let completion = recovered
            .begin_open_recovery(package.actions(), checkpoint)
            .load_with_journal(&MissingDecoder, &restarted_codec);
        let outcome = recovered.finish_open(completion, None).unwrap();
        assert_eq!(outcome.revisions.aggregate, 2);
        let names = recovered
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
        assert!(names.contains(&"Second"));
        assert_eq!(
            recovered.session().journal_checkpoint().unwrap(),
            ProjectJournalCheckpoint {
                through_sequence: 2,
                project_revision: 2,
            }
        );
        assert!(matches!(
            recovered.journal_recovery_state().replay,
            ProjectJournalReplayState::Replayed {
                from_revision: 1,
                through_revision: 2,
                through_sequence: 2,
                records: 1,
            }
        ));
    }

    #[test]
    fn long_session_rotates_checkpoints_compacts_journals_and_restarts_exactly() {
        let package = TempPackage::new("journal-long-session");
        seed(
            &package,
            &DawProject::new("journal long session", 48_000, 120.0).unwrap(),
        );
        let writer_codec = DeterministicRuntimeCommandCodec;
        let mut writer = open(&package);
        for index in 1..=12 {
            add_bus(&mut writer, &format!("Bus {index}"));
            let completion = writer
                .begin_autosave(1_000 + index)
                .unwrap()
                .persist_with_journal(&writer_codec);
            writer.finish_save(completion).unwrap();
            assert!(writer.session().journal_records().unwrap().is_empty());
        }

        let discovery = package.actions().recovery_options();
        assert_eq!(
            discovery.checkpoints.len(),
            crate::project_store::DEFAULT_MAX_RECOVERY_CHECKPOINTS
        );
        assert!(
            discovery.journals.len() <= crate::project_store::DEFAULT_MAX_ACTIVE_JOURNAL_SEGMENTS
        );
        assert!(discovery
            .journal_candidates
            .iter()
            .all(|candidate| candidate.tail == crate::command_journal::JournalTail::Clean));
        assert!(discovery
            .journal_candidates
            .iter()
            .any(|candidate| candidate.label.contains("Commands 1")));

        let mut restarted = TestDocument::new(92);
        let fresh_codec = DeterministicRuntimeCommandCodec;
        let completion = restarted
            .begin_open_primary(package.actions())
            .load_with_journal(&MissingDecoder, &fresh_codec);
        let outcome = restarted.finish_open(completion, None).unwrap();
        assert_eq!(outcome.revisions.aggregate, 12);
        assert_eq!(restarted.journal_recovery_state().replayed_records, 12);
        assert_eq!(
            restarted
                .session()
                .project_snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .mixer
                .buses()
                .count(),
            13
        );
        assert_eq!(
            restarted.preserved().sections["vendor.future"].bytes,
            vec![9, 8, 7]
        );
    }

    #[test]
    fn restart_replays_verified_prefix_and_labels_crash_tail() {
        let package = TempPackage::new("journal-crash-tail");
        seed(
            &package,
            &DawProject::new("journal crash tail", 48_000, 120.0).unwrap(),
        );
        let codec = DeterministicRuntimeCommandCodec;
        let mut writer = open(&package);
        add_bus(&mut writer, "Durable");
        let completion = writer
            .begin_autosave(2_000)
            .unwrap()
            .persist_with_journal(&codec);
        writer.finish_save(completion).unwrap();
        let segment = package.actions().recovery_options().journals[0].clone();
        let mut bytes = fs::read(&segment).unwrap();
        bytes.extend_from_slice(b"AUDEC");
        fs::write(&segment, bytes).unwrap();

        let discovery = package.actions().recovery_options();
        assert_eq!(discovery.journal_candidates.len(), 1);
        assert_eq!(discovery.journal_candidates[0].verified_frames, 1);
        assert!(discovery.journal_candidates[0]
            .label
            .contains("crash-truncated tail"));

        let mut restarted = TestDocument::new(93);
        let fresh_codec = DeterministicRuntimeCommandCodec;
        let completion = restarted
            .begin_open_primary(package.actions())
            .load_with_journal(&MissingDecoder, &fresh_codec);
        let outcome = restarted.finish_open(completion, None).unwrap();
        assert_eq!(outcome.revisions.aggregate, 1);
        assert_eq!(restarted.journal_recovery_state().replayed_records, 1);
        assert!(matches!(
            restarted.journal_recovery_state().replay,
            ProjectJournalReplayState::Partial { records: 1, .. }
        ));
        assert!(restarted
            .journal_recovery_state()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("rejected tail")));
    }

    #[test]
    fn journal_asset_replay_rematerializes_pcm_before_publication() {
        let package = TempPackage::new("journal-asset-pcm");
        seed(
            &package,
            &DawProject::new("journal asset", 48_000, 120.0).unwrap(),
        );
        let media_path = package.path.join("recoverable.raw");
        fs::write(&media_path, b"exact recoverable media").unwrap();
        let metadata = DecodedAudioMetadata {
            sample_rate_hz: 48_000,
            channels: 1,
            frame_count: SampleFrames(4),
            container: Some("raw".into()),
            codec: Some("pcm_f32le".into()),
            bit_depth: Some(32),
        };
        let fingerprint = ContentFingerprint::from_bytes(b"exact recoverable media");
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.25_f32, -0.5, 0.75, 0.0]),
        )
        .unwrap();
        let decoder = ExactDecoder {
            metadata: metadata.clone(),
            fingerprint,
            pcm: pcm.clone(),
        };
        let absolute = AbsolutePath::parse(media_path.to_string_lossy().into_owned()).unwrap();
        let location = AssetLocation::new(Some(absolute), None).unwrap();
        let registration = AssetRegistration {
            name: "recoverable.raw".into(),
            location: location.clone(),
            metadata,
            content: fingerprint,
            provenance: AssetProvenance::new(
                40,
                AssetOrigin::ImportedFile {
                    importer: "lifecycle test".into(),
                },
                location,
            ),
            tags: BTreeSet::new(),
            favorite: false,
        };
        let writer_codec = DeterministicRuntimeCommandCodec;
        let mut writer = open(&package);
        let imported = writer
            .session_mut()
            .import_asset(0, registration, pcm.clone())
            .unwrap();
        let asset = imported.asset;
        writer
            .session_mut()
            .publish_workbench_range(
                asset,
                SampleRange::new(Sample::new(0), Sample::new(4)),
                WorkbenchSampleIntent::OneShot {
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                },
            )
            .unwrap();
        let completion = writer
            .begin_autosave(41)
            .unwrap()
            .persist_with_journal(&writer_codec);
        writer.finish_save(completion).unwrap();

        let mut recovered = TestDocument::new(89);
        let restarted_codec = DeterministicRuntimeCommandCodec;
        let completion = recovered
            .begin_open_primary(package.actions())
            .load_with_journal(&decoder, &restarted_codec);
        let outcome = recovered.finish_open(completion, None).unwrap();
        assert_eq!(outcome.revisions.aggregate, 2);
        let snapshot = recovered.session().project_snapshot().unwrap();
        assert!(snapshot.project.state().domains.assets.get(asset).is_some());
        let recovered_pcm = &snapshot.pcm[&asset];
        assert_eq!(recovered_pcm.format, pcm.format);
        assert_eq!(recovered_pcm.samples.as_ref(), pcm.samples.as_ref());
        assert_eq!(snapshot.sample_pcm.len(), 1);
        let materialized = snapshot.sample_pcm.values().next().unwrap();
        assert_eq!(materialized.samples.as_ref(), pcm.samples.as_ref());
        assert_eq!(recovered.journal_recovery_state().replayed_records, 2);
        assert!(recovered.journal_recovery_state().diagnostics.is_empty());

        // A different process with no media decoder keeps the known-good
        // checkpoint and foreign payloads intact, and reports why the journal
        // suffix was not publishable instead of fabricating silent PCM.
        let mut missing_media = TestDocument::new(91);
        let fresh_codec = DeterministicRuntimeCommandCodec;
        let completion = missing_media
            .begin_open_primary(package.actions())
            .load_with_journal(&MissingDecoder, &fresh_codec);
        let outcome = missing_media.finish_open(completion, None).unwrap();
        assert_eq!(outcome.revisions.aggregate, 0);
        assert_eq!(missing_media.journal_recovery_state().replayed_records, 0);
        assert!(missing_media
            .journal_recovery_state()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("exact PCM could not be rematerialized")));
        assert_eq!(
            missing_media.preserved().envelope_extensions["vendor.note"],
            serde_json::Value::String("keep".into())
        );
        assert_eq!(
            missing_media.preserved().sections["vendor.future"].bytes,
            vec![9, 8, 7]
        );
    }
}
