//! Pure ownership model for application, project-session, and project-window
//! lifetimes.
//!
//! GPUI wiring should mirror this graph without storing widget handles here:
//!
//! ```text
//! ApplicationController entity
//!   -> ProjectWindow root entity
//!        -> ProjectSession entity
//!        -> WorkspaceRoot entity
//! ```
//!
//! `ProjectSession` never owns either root. A workspace pane may own a strong
//! project-session entity handle; callbacks toward the application/window use
//! weak handles. This prevents the Workbench-style view/project ownership
//! cycle while allowing several windows to present one project.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::project_session::ProjectSessionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectWindowId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceInstanceId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectWindowRole {
    Primary,
    Auxiliary,
}

/// Pure binding record. Runtime GPUI entities are stored by the application
/// host in maps keyed by these IDs, never inside the project session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectWindowBinding {
    pub id: ProjectWindowId,
    pub session: ProjectSessionId,
    pub workspace: WorkspaceInstanceId,
    pub role: ProjectWindowRole,
}

#[derive(Clone, Debug)]
pub struct ApplicationControllerModel {
    sessions: BTreeSet<ProjectSessionId>,
    windows: BTreeMap<ProjectWindowId, ProjectWindowBinding>,
    next_session: u64,
    next_window: u64,
    next_workspace: u64,
}

impl Default for ApplicationControllerModel {
    fn default() -> Self {
        Self {
            sessions: BTreeSet::new(),
            windows: BTreeMap::new(),
            next_session: 1,
            next_window: 1,
            next_workspace: 1,
        }
    }
}

impl ApplicationControllerModel {
    pub fn sessions(&self) -> impl ExactSizeIterator<Item = ProjectSessionId> + '_ {
        self.sessions.iter().copied()
    }

    pub fn windows(&self) -> impl ExactSizeIterator<Item = &ProjectWindowBinding> {
        self.windows.values()
    }

    pub fn create_session(&mut self) -> Result<ProjectSessionId, ApplicationOwnershipError> {
        let id = ProjectSessionId(self.next_session);
        self.next_session = checked_next(self.next_session)?;
        self.sessions.insert(id);
        Ok(id)
    }

    pub fn insert_session(
        &mut self,
        id: ProjectSessionId,
    ) -> Result<(), ApplicationOwnershipError> {
        if id.0 == 0 {
            return Err(ApplicationOwnershipError::ZeroId);
        }
        if !self.sessions.insert(id) {
            return Err(ApplicationOwnershipError::DuplicateSession(id));
        }
        self.next_session = self.next_session.max(checked_next(id.0)?);
        Ok(())
    }

    pub fn open_window(
        &mut self,
        session: ProjectSessionId,
        role: ProjectWindowRole,
    ) -> Result<ProjectWindowBinding, ApplicationOwnershipError> {
        if !self.sessions.contains(&session) {
            return Err(ApplicationOwnershipError::UnknownSession(session));
        }
        if role == ProjectWindowRole::Primary
            && self
                .windows
                .values()
                .any(|window| window.session == session && window.role == role)
        {
            return Err(ApplicationOwnershipError::DuplicatePrimary(session));
        }
        let binding = ProjectWindowBinding {
            id: ProjectWindowId(self.next_window),
            session,
            workspace: WorkspaceInstanceId(self.next_workspace),
            role,
        };
        self.next_window = checked_next(self.next_window)?;
        self.next_workspace = checked_next(self.next_workspace)?;
        self.windows.insert(binding.id, binding);
        Ok(binding)
    }

    pub fn close_window(
        &mut self,
        id: ProjectWindowId,
    ) -> Result<ProjectWindowBinding, ApplicationOwnershipError> {
        self.windows
            .remove(&id)
            .ok_or(ApplicationOwnershipError::UnknownWindow(id))
    }

    pub fn close_session(&mut self, id: ProjectSessionId) -> Result<(), ApplicationOwnershipError> {
        if self.windows.values().any(|window| window.session == id) {
            return Err(ApplicationOwnershipError::SessionHasWindows(id));
        }
        if !self.sessions.remove(&id) {
            return Err(ApplicationOwnershipError::UnknownSession(id));
        }
        Ok(())
    }

    pub fn binding(&self, id: ProjectWindowId) -> Option<ProjectWindowBinding> {
        self.windows.get(&id).copied()
    }
}

fn checked_next(value: u64) -> Result<u64, ApplicationOwnershipError> {
    value
        .checked_add(1)
        .ok_or(ApplicationOwnershipError::IdExhausted)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplicationOwnershipError {
    ZeroId,
    DuplicateSession(ProjectSessionId),
    UnknownSession(ProjectSessionId),
    UnknownWindow(ProjectWindowId),
    DuplicatePrimary(ProjectSessionId),
    SessionHasWindows(ProjectSessionId),
    IdExhausted,
}

impl fmt::Display for ApplicationOwnershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroId => formatter.write_str("application identity zero is reserved"),
            Self::DuplicateSession(id) => {
                write!(formatter, "project session {} already exists", id.0)
            }
            Self::UnknownSession(id) => write!(formatter, "project session {} is unknown", id.0),
            Self::UnknownWindow(id) => write!(formatter, "project window {} is unknown", id.0),
            Self::DuplicatePrimary(id) => {
                write!(
                    formatter,
                    "project session {} already has a primary window",
                    id.0
                )
            }
            Self::SessionHasWindows(id) => write!(
                formatter,
                "project session {} still has project windows",
                id.0
            ),
            Self::IdExhausted => formatter.write_str("application controller IDs are exhausted"),
        }
    }
}

impl Error for ApplicationOwnershipError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_session_can_have_primary_and_auxiliary_workspaces() {
        let mut app = ApplicationControllerModel::default();
        let session = app.create_session().unwrap();
        let primary = app
            .open_window(session, ProjectWindowRole::Primary)
            .unwrap();
        let auxiliary = app
            .open_window(session, ProjectWindowRole::Auxiliary)
            .unwrap();
        assert_ne!(primary.workspace, auxiliary.workspace);
        assert_eq!(app.windows().count(), 2);
        assert_eq!(
            app.close_session(session),
            Err(ApplicationOwnershipError::SessionHasWindows(session))
        );
    }

    #[test]
    fn primary_window_is_unique_per_session() {
        let mut app = ApplicationControllerModel::default();
        let session = app.create_session().unwrap();
        app.open_window(session, ProjectWindowRole::Primary)
            .unwrap();
        assert_eq!(
            app.open_window(session, ProjectWindowRole::Primary),
            Err(ApplicationOwnershipError::DuplicatePrimary(session))
        );
    }
}
