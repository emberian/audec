//! UI-neutral file-operation facade for project packages.
//!
//! Dialogs, keyboard shortcuts, and notifications belong in the application
//! adapter.  This module gives each of those surfaces one small, owned-data
//! API instead of letting them independently coordinate codecs, package
//! storage, media resolution, recovery selection, and revision-pinned export.
//! It intentionally does not mutate a `LiveProject`: a controller supplies a
//! snapshot and applies the returned save guard or opened aggregate at its own
//! revision boundary.

use std::path::PathBuf;

use crate::daw_project::DawProject;
use crate::export::{
    ExportObserver, RevisionPinnedAudio, RevisionPinnedWavExportReport, WavExportRequest,
};
use crate::media_resolver::MediaDecoder;
use crate::project_format::PreservedProjectData;
use crate::project_repository::{
    AirPayloadCodec, MediaHydration, OpenedProject, ProjectRepository, ProjectRepositoryError,
    RecoveryPreference,
};
use crate::project_store::{RecoveryCheckpoint, RecoveryDiscovery, SaveResult};
use crate::workspace_document::WorkspaceDocument;

/// The application-facing file service.  It is deliberately a thin facade:
/// all byte-format authority stays in [`ProjectRepository`], while a later
/// GPUI adapter only has to translate native dialogs into calls on this type.
#[derive(Clone, Debug)]
pub struct ProjectFileActions<C> {
    repository: ProjectRepository<C>,
}

impl<C> ProjectFileActions<C>
where
    C: AirPayloadCodec,
{
    pub fn new(repository: ProjectRepository<C>) -> Self {
        Self { repository }
    }

    pub fn repository(&self) -> &ProjectRepository<C> {
        &self.repository
    }

    /// Atomically save one immutable snapshot.  The returned revision guard
    /// is intentionally not applied here: the live controller must compare it
    /// against the revision it has by the time worker I/O completes.
    pub fn save(
        &self,
        project: &DawProject,
        preserved: PreservedProjectData,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.repository.save_primary(project, preserved)
    }

    /// Save the aggregate and its complete dynamic workspace as one durable
    /// checkpoint. The document is data-only and can be captured before a
    /// background save without retaining a GPUI entity.
    pub fn save_with_workspace(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.repository
            .save_primary_with_workspace(project, workspace, preserved)
    }

    /// Publish a labelled recovery checkpoint without changing the primary
    /// manifest or a document's clean/dirty state.
    pub fn autosave(
        &self,
        project: &DawProject,
        preserved: PreservedProjectData,
        saved_unix_ms: u64,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.repository
            .save_autosave(project, preserved, saved_unix_ms)
    }

    pub fn autosave_with_workspace(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
        saved_unix_ms: u64,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.repository
            .save_autosave_with_workspace(project, workspace, preserved, saved_unix_ms)
    }

    pub fn recovery_options(&self) -> RecoveryDiscovery {
        self.repository.recovery_discovery()
    }

    /// A labeled recommendation for recovery UI. It never restores anything
    /// implicitly; use [`open_recovery`] only after an explicit choice.
    pub fn recovery_preference(&self) -> Result<RecoveryPreference, ProjectRepositoryError> {
        self.repository.recovery_preference()
    }

    /// Persist an opaque command-journal segment supplied by the envelope
    /// service.  File actions intentionally do not interpret or replay it.
    pub fn write_journal_segment(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, ProjectRepositoryError> {
        self.repository.write_journal_segment(name, bytes)
    }

    pub fn open(&self) -> Result<OpenedProject, ProjectRepositoryError> {
        self.repository.open_primary()
    }

    /// A recovery choice is explicit and preserves its own labelled
    /// provenance.  The app decides whether to offer it, not the filesystem.
    pub fn open_recovery(
        &self,
        checkpoint: &RecoveryCheckpoint,
    ) -> Result<OpenedProject, ProjectRepositoryError> {
        self.repository.open_recovery(checkpoint)
    }

    /// Hydration is a second, independent phase after opening.  A project can
    /// be inspected and saved even if source media has moved; callers surface
    /// the returned relink proposals and only issue a binding-changing command
    /// after an explicit user decision.
    pub fn hydrate(&self, project: &DawProject, decoder: &impl MediaDecoder) -> MediaHydration {
        self.repository.hydrate_media(project, decoder)
    }

    /// Export the immutable render captured at a known aggregate revision.
    /// The report retains that revision; it must never be relabelled as a
    /// later, currently edited project state.
    pub fn export<O: ExportObserver>(
        &self,
        pinned: RevisionPinnedAudio,
        request: &WavExportRequest,
        observer: &mut O,
    ) -> Result<RevisionPinnedWavExportReport, ProjectRepositoryError> {
        self.repository
            .export_revision_pinned(pinned, request, observer)
    }
}
