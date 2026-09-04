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

use crate::daw_engine::AssetPcmMap;
use crate::daw_project::DawProject;
use crate::export::{
    ExportObserver, RevisionPinnedAudio, RevisionPinnedWavExportReport, WavExportRequest,
};
use crate::media_resolver::MediaDecoder;
use crate::project_format::PreservedProjectData;
use crate::project_repository::{
    MediaHydration, OpenedProject, ProjectRepository, ProjectRepositoryError, RecoveryPreference,
};
use crate::project_store::{
    JournalCompactionResult, RecoveryCheckpoint, RecoveryDiscovery, SaveResult,
};
use crate::ui_actions::{ActionId, ActionRequest, FileActionIntent, ProductActionIntent};
use crate::workspace_document::WorkspaceDocument;

/// Host interaction required to finish a file action. The registry chooses
/// the product intent; the GPUI host owns dialogs, dirty-project confirmation,
/// window lifecycle and application shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileActionInteraction {
    ReplaceProject,
    SaveCurrent,
    ChooseSaveDestination,
    ChooseRecovery,
    ChooseExportDestination,
    RequestApplicationQuit,
}

/// Epoch-bearing file intent after it has been lowered out of the shared
/// action vocabulary. Keeping the original request means native menu,
/// palette, shortcut and accessibility callbacks all reach the same stale
/// projection check before the host does any I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileActionDispatchPlan {
    pub request: ActionRequest,
    pub intent: FileActionIntent,
    pub interaction: FileActionInteraction,
    pub requires_dirty_project_guard: bool,
    pub requires_project_snapshot: bool,
}

pub fn plan_file_action(
    request: &ActionRequest,
) -> Result<FileActionDispatchPlan, FileActionPlanError> {
    let action = request.invocation.action;
    let Some(ProductActionIntent::File(intent)) = ProductActionIntent::from_action(action) else {
        return Err(FileActionPlanError::NotAFileAction(action));
    };
    let (interaction, requires_dirty_project_guard, requires_project_snapshot) = match intent {
        FileActionIntent::NewProject
        | FileActionIntent::OpenProject
        | FileActionIntent::OpenAudio => (FileActionInteraction::ReplaceProject, true, false),
        FileActionIntent::Save => (FileActionInteraction::SaveCurrent, false, true),
        FileActionIntent::SaveAs => (FileActionInteraction::ChooseSaveDestination, false, true),
        FileActionIntent::OpenRecovery => (FileActionInteraction::ChooseRecovery, true, false),
        FileActionIntent::ExportAudio => {
            (FileActionInteraction::ChooseExportDestination, false, true)
        }
        FileActionIntent::Quit => (FileActionInteraction::RequestApplicationQuit, true, false),
    };
    Ok(FileActionDispatchPlan {
        request: request.clone(),
        intent,
        interaction,
        requires_dirty_project_guard,
        requires_project_snapshot,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileActionPlanError {
    NotAFileAction(ActionId),
}

/// The application-facing file service.  It is deliberately a thin facade:
/// all byte-format authority stays in [`ProjectRepository`], while a later
/// GPUI adapter only has to translate native dialogs into calls on this type.
#[derive(Clone, Debug)]
pub struct ProjectFileActions {
    repository: ProjectRepository,
}

impl ProjectFileActions {
    pub fn new(repository: ProjectRepository) -> Self {
        Self { repository }
    }

    pub fn repository(&self) -> &ProjectRepository {
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

    pub fn save_with_workspace_and_media(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
        pcm: &AssetPcmMap,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.repository
            .save_primary_with_workspace_and_media(project, workspace, preserved, pcm)
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

    pub fn autosave_with_workspace_and_media(
        &self,
        project: &DawProject,
        workspace: Option<&WorkspaceDocument>,
        preserved: PreservedProjectData,
        saved_unix_ms: u64,
        pcm: &AssetPcmMap,
    ) -> Result<SaveResult, ProjectRepositoryError> {
        self.repository.save_autosave_with_workspace_and_media(
            project,
            workspace,
            preserved,
            saved_unix_ms,
            pcm,
        )
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

    /// Compact older verified frames into the existing journal format while
    /// retaining recent segments for cheap incremental autosaves.
    pub fn compact_journal_segments(
        &self,
        max_active_segments: usize,
    ) -> Result<JournalCompactionResult, ProjectRepositoryError> {
        self.repository
            .compact_journal_segments(max_active_segments)
    }

    /// Explicit maintenance hook for hosts which want a retention policy
    /// different from the store's default autosave rotation.
    pub fn rotate_recovery_checkpoints(
        &self,
        max_checkpoints: usize,
    ) -> Result<Vec<PathBuf>, ProjectRepositoryError> {
        self.repository.rotate_recovery_checkpoints(max_checkpoints)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_actions::{
        ids, ActionInvocation, ActionParameters, ContextEpoch, InvocationModifiers,
        InvocationOrigin, ProjectionEpoch, RegistryEpoch,
    };

    fn request(action: ActionId) -> ActionRequest {
        ActionRequest {
            invocation: ActionInvocation {
                action,
                origin: InvocationOrigin::Menu,
                view: None,
                target: None,
                modifiers: InvocationModifiers::default(),
            },
            parameters: ActionParameters::default(),
            projected_at: ProjectionEpoch {
                registry: RegistryEpoch(4),
                context: ContextEpoch(9),
            },
        }
    }

    #[test]
    fn file_plans_keep_lifecycle_guards_and_snapshot_requirements_explicit() {
        let cases = [
            (
                ids::FILE_NEW,
                FileActionInteraction::ReplaceProject,
                true,
                false,
            ),
            (
                ids::FILE_OPEN_AUDIO,
                FileActionInteraction::ReplaceProject,
                true,
                false,
            ),
            (
                ids::FILE_SAVE,
                FileActionInteraction::SaveCurrent,
                false,
                true,
            ),
            (
                ids::FILE_SAVE_AS,
                FileActionInteraction::ChooseSaveDestination,
                false,
                true,
            ),
            (
                ids::FILE_RECOVERY,
                FileActionInteraction::ChooseRecovery,
                true,
                false,
            ),
            (
                ids::FILE_EXPORT,
                FileActionInteraction::ChooseExportDestination,
                false,
                true,
            ),
            (
                ids::FILE_QUIT,
                FileActionInteraction::RequestApplicationQuit,
                true,
                false,
            ),
        ];
        for (action, interaction, dirty_guard, snapshot) in cases {
            let plan = plan_file_action(&request(action)).unwrap();
            assert_eq!(plan.interaction, interaction);
            assert_eq!(plan.requires_dirty_project_guard, dirty_guard);
            assert_eq!(plan.requires_project_snapshot, snapshot);
            assert_eq!(plan.request.projected_at.context, ContextEpoch(9));
        }
    }

    #[test]
    fn a_plan_carries_the_exact_epoch_bearing_request() {
        let request = request(ids::FILE_SAVE_AS);
        let plan = plan_file_action(&request).unwrap();
        assert_eq!(plan.request, request);
        assert_eq!(plan.intent, FileActionIntent::SaveAs);
    }

    #[test]
    fn non_file_actions_are_refused_before_any_plan_exists() {
        assert_eq!(
            plan_file_action(&request(ids::EDIT_UNDO)),
            Err(FileActionPlanError::NotAFileAction(ids::EDIT_UNDO))
        );
    }
}
