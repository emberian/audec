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

use gpui::{AnyWindowHandle, App, AppContext as _, Entity};

use crate::project_session::{ProjectSession, ProjectSessionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectWindowId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceInstanceId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectWindowRole {
    Primary,
    Auxiliary,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LastProjectWindowPolicy {
    #[default]
    Terminate,
    KeepApplicationAlive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationLifecycleEffect {
    None,
    QuitApplication,
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
    last_window_policy: LastProjectWindowPolicy,
}

impl Default for ApplicationControllerModel {
    fn default() -> Self {
        Self {
            sessions: BTreeSet::new(),
            windows: BTreeMap::new(),
            next_session: 1,
            next_window: 1,
            next_workspace: 1,
            last_window_policy: LastProjectWindowPolicy::default(),
        }
    }
}

impl ApplicationControllerModel {
    pub const fn last_window_policy(&self) -> LastProjectWindowPolicy {
        self.last_window_policy
    }

    pub fn set_last_window_policy(&mut self, policy: LastProjectWindowPolicy) {
        self.last_window_policy = policy;
    }

    pub fn sessions(&self) -> impl ExactSizeIterator<Item = ProjectSessionId> + '_ {
        self.sessions.iter().copied()
    }

    pub fn windows(&self) -> impl ExactSizeIterator<Item = &ProjectWindowBinding> {
        self.windows.values()
    }

    pub fn windows_for_session(
        &self,
        session: ProjectSessionId,
    ) -> impl Iterator<Item = &ProjectWindowBinding> {
        self.windows
            .values()
            .filter(move |window| window.session == session)
    }

    pub fn primary_window(&self, session: ProjectSessionId) -> Option<ProjectWindowBinding> {
        self.windows_for_session(session)
            .find(|window| window.role == ProjectWindowRole::Primary)
            .copied()
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
        self.close_window_with_effect(id)
            .map(|(binding, _)| binding)
    }

    pub fn close_window_with_effect(
        &mut self,
        id: ProjectWindowId,
    ) -> Result<(ProjectWindowBinding, ApplicationLifecycleEffect), ApplicationOwnershipError> {
        let binding = self
            .windows
            .remove(&id)
            .ok_or(ApplicationOwnershipError::UnknownWindow(id))?;
        let effect = if self.windows.is_empty()
            && self.last_window_policy == LastProjectWindowPolicy::Terminate
        {
            ApplicationLifecycleEffect::QuitApplication
        } else {
            ApplicationLifecycleEffect::None
        };
        Ok((binding, effect))
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
    WindowAlreadyAttached(ProjectWindowId),
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
            Self::WindowAlreadyAttached(id) => {
                write!(
                    formatter,
                    "project window {} already has a native handle",
                    id.0
                )
            }
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

/// GPUI-side owner for the pure application graph. Project sessions are
/// entities because editors and several native windows observe the same
/// publication; window roots are held only by native handles and never by the
/// session, preventing a session/view ownership cycle.
pub struct ApplicationController {
    model: ApplicationControllerModel,
    sessions: BTreeMap<ProjectSessionId, Entity<ProjectSession>>,
    windows: BTreeMap<ProjectWindowId, AnyWindowHandle>,
}

impl Default for ApplicationController {
    fn default() -> Self {
        Self {
            model: ApplicationControllerModel::default(),
            sessions: BTreeMap::new(),
            windows: BTreeMap::new(),
        }
    }
}

impl ApplicationController {
    pub fn model(&self) -> &ApplicationControllerModel {
        &self.model
    }

    pub fn create_session_entity(
        &mut self,
        cx: &mut App,
    ) -> Result<(ProjectSessionId, Entity<ProjectSession>), ApplicationOwnershipError> {
        let id = self.model.create_session()?;
        let session = cx.new(|_| {
            ProjectSession::new(id).expect("application allocator never produces session ID zero")
        });
        self.sessions.insert(id, session.clone());
        Ok((id, session))
    }

    pub fn insert_session(
        &mut self,
        session: Entity<ProjectSession>,
        id: ProjectSessionId,
    ) -> Result<(), ApplicationOwnershipError> {
        self.model.insert_session(id)?;
        self.sessions.insert(id, session);
        Ok(())
    }

    pub fn session(&self, id: ProjectSessionId) -> Option<Entity<ProjectSession>> {
        self.sessions.get(&id).cloned()
    }

    /// Reserve the durable ownership edge before opening a native window. If
    /// GPUI window creation fails, call [`abandon_window`](Self::abandon_window)
    /// so the pure graph never claims a window that does not exist.
    pub fn reserve_window(
        &mut self,
        session: ProjectSessionId,
        role: ProjectWindowRole,
    ) -> Result<ProjectWindowBinding, ApplicationOwnershipError> {
        self.model.open_window(session, role)
    }

    pub fn attach_window(
        &mut self,
        id: ProjectWindowId,
        handle: AnyWindowHandle,
    ) -> Result<(), ApplicationOwnershipError> {
        if self.model.binding(id).is_none() {
            return Err(ApplicationOwnershipError::UnknownWindow(id));
        }
        if self.windows.contains_key(&id) {
            return Err(ApplicationOwnershipError::WindowAlreadyAttached(id));
        }
        self.windows.insert(id, handle);
        Ok(())
    }

    pub fn window(&self, id: ProjectWindowId) -> Option<AnyWindowHandle> {
        self.windows.get(&id).copied()
    }

    pub fn session_for_window(&self, id: ProjectWindowId) -> Option<Entity<ProjectSession>> {
        let binding = self.model.binding(id)?;
        self.session(binding.session)
    }

    pub fn detach_window(
        &mut self,
        id: ProjectWindowId,
    ) -> Result<ProjectWindowBinding, ApplicationOwnershipError> {
        if self.model.binding(id).is_none() {
            return Err(ApplicationOwnershipError::UnknownWindow(id));
        }
        self.windows.remove(&id);
        self.model.close_window(id)
    }

    pub fn detach_window_with_effect(
        &mut self,
        id: ProjectWindowId,
    ) -> Result<(ProjectWindowBinding, ApplicationLifecycleEffect), ApplicationOwnershipError> {
        if self.model.binding(id).is_none() {
            return Err(ApplicationOwnershipError::UnknownWindow(id));
        }
        self.windows.remove(&id);
        self.model.close_window_with_effect(id)
    }

    pub fn abandon_window(
        &mut self,
        id: ProjectWindowId,
    ) -> Result<ProjectWindowBinding, ApplicationOwnershipError> {
        if self.windows.contains_key(&id) {
            return Err(ApplicationOwnershipError::WindowAlreadyAttached(id));
        }
        self.model.close_window(id)
    }

    pub fn remove_session(
        &mut self,
        id: ProjectSessionId,
    ) -> Result<Entity<ProjectSession>, ApplicationOwnershipError> {
        self.model.close_session(id)?;
        self.sessions
            .remove(&id)
            .ok_or(ApplicationOwnershipError::UnknownSession(id))
    }
}

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

    #[test]
    fn session_window_queries_never_mix_project_ownership() {
        let mut app = ApplicationControllerModel::default();
        let first = app.create_session().unwrap();
        let second = app.create_session().unwrap();
        let primary = app.open_window(first, ProjectWindowRole::Primary).unwrap();
        app.open_window(first, ProjectWindowRole::Auxiliary)
            .unwrap();
        app.open_window(second, ProjectWindowRole::Primary).unwrap();

        assert_eq!(app.windows_for_session(first).count(), 2);
        assert_eq!(app.windows_for_session(second).count(), 1);
        assert_eq!(app.primary_window(first), Some(primary));
    }

    #[test]
    fn default_policy_quits_only_after_the_last_project_window_detaches() {
        let mut app = ApplicationControllerModel::default();
        let session = app.create_session().unwrap();
        let primary = app
            .open_window(session, ProjectWindowRole::Primary)
            .unwrap();
        let auxiliary = app
            .open_window(session, ProjectWindowRole::Auxiliary)
            .unwrap();

        assert_eq!(
            app.close_window_with_effect(auxiliary.id).unwrap().1,
            ApplicationLifecycleEffect::None
        );
        assert_eq!(
            app.close_window_with_effect(primary.id).unwrap().1,
            ApplicationLifecycleEffect::QuitApplication
        );
        assert_eq!(app.windows().count(), 0);
    }

    #[test]
    fn keep_alive_policy_is_explicit() {
        let mut app = ApplicationControllerModel::default();
        app.set_last_window_policy(LastProjectWindowPolicy::KeepApplicationAlive);
        let session = app.create_session().unwrap();
        let window = app
            .open_window(session, ProjectWindowRole::Primary)
            .unwrap();
        assert_eq!(
            app.close_window_with_effect(window.id).unwrap().1,
            ApplicationLifecycleEffect::None
        );
    }
}
