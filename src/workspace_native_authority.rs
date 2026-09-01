//! Transactional authority between the portable workspace and native windows.
//!
//! Commands mutate [`WorkspaceSessionLayout`] first. The UI then actuates the
//! returned effects and either acknowledges the token or reports a native
//! failure. A failure restores the complete portable document and yields an
//! explicit reconciliation plan; native state is never allowed to become the
//! source of truth by being mirrored back into the document.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::pane_session_binding::{PaneSessionRegistration, PaneSessionTopics};
use crate::workspace_document::{
    DockLayout, WindowPlacement, WorkspaceDocument, WorkspaceWindowId,
};
use crate::workspace_session_layout::{
    NativeWindowEffect, PaneBindingEffect, PaneInstanceId, PaneMoveDestination,
    PanePresentationMemory, WorkspaceLayoutTransition, WorkspaceSessionLayout,
    WorkspaceSessionLayoutError, WorkspaceWindow,
};

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceLayoutCommand {
    /// Replace descriptor/catalog presentation as one portable command. This
    /// is used for dynamic view creation/removal and project reopen; native
    /// effects are derived from the before/after documents.
    ReplaceDocument {
        document: WorkspaceDocument,
    },
    ReplaceWindowLayout {
        window: WorkspaceWindow,
        layout: DockLayout,
    },
    SetWindowPlacement {
        window: WorkspaceWindow,
        placement: Option<WindowPlacement>,
    },
    UpdatePresentationMemory {
        pane: PaneInstanceId,
        memory: PanePresentationMemory,
    },
    FocusPane(PaneInstanceId),
    MovePane {
        pane: PaneInstanceId,
        destination: PaneMoveDestination,
    },
    TearOffPane {
        pane: PaneInstanceId,
        placement: Option<WindowPlacement>,
    },
    CloseTab(PaneInstanceId),
    ReopenTab(PaneInstanceId),
    DockWindow(WorkspaceWindowId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceActuationToken(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub struct AcceptedWorkspaceCommand {
    pub token: WorkspaceActuationToken,
    /// Monotonic authority revision, including rollbacks.
    pub authority_revision: u64,
    pub transition: WorkspaceLayoutTransition,
    pub document: WorkspaceDocument,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceNativeFailure {
    pub effect_index: usize,
    pub operation: WorkspaceNativeOperation,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceNativeOperation {
    ApplyDocument,
    ApplyBinding,
    ApplyWindow,
    RestoreDocument,
    RestoreBinding,
    RestoreWindow,
}

impl WorkspaceNativeOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApplyDocument => "apply_document",
            Self::ApplyBinding => "apply_binding",
            Self::ApplyWindow => "apply_window",
            Self::RestoreDocument => "restore_document",
            Self::RestoreBinding => "restore_binding",
            Self::RestoreWindow => "restore_window",
        }
    }
}

impl fmt::Display for WorkspaceNativeOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceRollback {
    pub failed_token: WorkspaceActuationToken,
    pub authority_revision: u64,
    pub document: WorkspaceDocument,
    pub bindings: Vec<PaneBindingEffect>,
    pub windows: Vec<NativeWindowEffect>,
    pub failure: WorkspaceNativeFailure,
}

/// Toolkit adapter invoked only after a portable command has been accepted.
/// Implementations should restore pane trees in `apply_document`, then perform
/// binding and native-window effects exactly in the supplied order.
pub trait WorkspaceNativeActuator {
    type Error: fmt::Display;

    fn apply_document(&mut self, document: &WorkspaceDocument) -> Result<(), Self::Error>;
    fn apply_binding(&mut self, effect: PaneBindingEffect) -> Result<(), Self::Error>;
    fn apply_window(&mut self, effect: NativeWindowEffect) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceActuationDiagnostic {
    pub operation: WorkspaceNativeOperation,
    pub effect_index: usize,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceExecutionFailure {
    pub rollback: WorkspaceRollback,
    /// Failures encountered while reconciling native state back to the
    /// restored portable document. The portable rollback is complete even if
    /// a dead native window can no longer be contacted.
    pub recovery_diagnostics: Vec<WorkspaceActuationDiagnostic>,
}

#[derive(Clone, Debug)]
struct PendingActuation {
    token: WorkspaceActuationToken,
    before: WorkspaceSessionLayout,
    accepted: WorkspaceDocument,
    transition: WorkspaceLayoutTransition,
}

/// Serial command gate for one project workspace. Only one command may be in
/// native actuation at a time, which makes partial-window failure reversible.
#[derive(Clone, Debug)]
pub struct WorkspaceCommandAuthority {
    layout: WorkspaceSessionLayout,
    authority_revision: u64,
    next_token: u64,
    pending: Option<PendingActuation>,
}

impl WorkspaceCommandAuthority {
    pub fn new(layout: WorkspaceSessionLayout) -> Self {
        Self {
            authority_revision: layout.revision(),
            layout,
            next_token: 1,
            pending: None,
        }
    }

    pub const fn revision(&self) -> u64 {
        self.authority_revision
    }

    pub fn layout(&self) -> &WorkspaceSessionLayout {
        &self.layout
    }

    pub fn document(&self) -> &WorkspaceDocument {
        self.layout.document()
    }

    pub fn export_document(&self) -> Result<WorkspaceDocument, WorkspaceAuthorityError> {
        self.layout.export_document().map_err(Into::into)
    }

    pub fn has_pending_actuation(&self) -> bool {
        self.pending.is_some()
    }

    pub fn accept(
        &mut self,
        expected_revision: u64,
        command: WorkspaceLayoutCommand,
    ) -> Result<AcceptedWorkspaceCommand, WorkspaceAuthorityError> {
        if let Some(pending) = &self.pending {
            return Err(WorkspaceAuthorityError::ActuationPending(pending.token));
        }
        if expected_revision != self.authority_revision {
            return Err(WorkspaceAuthorityError::StaleRevision {
                expected: expected_revision,
                actual: self.authority_revision,
            });
        }

        let before = self.layout.clone();
        let mut transition = match command {
            WorkspaceLayoutCommand::ReplaceDocument { document } => {
                let next =
                    WorkspaceSessionLayout::from_document(self.layout.session_id(), document)?;
                let transition = replacement_transition(
                    &self.layout,
                    &next,
                    self.authority_revision.wrapping_add(1).max(1),
                );
                self.layout = next;
                transition
            }
            WorkspaceLayoutCommand::ReplaceWindowLayout { window, layout } => {
                self.layout.replace_window_layout(window, layout)?
            }
            WorkspaceLayoutCommand::SetWindowPlacement { window, placement } => {
                self.layout.set_window_placement(window, placement)?
            }
            WorkspaceLayoutCommand::UpdatePresentationMemory { pane, memory } => {
                self.layout.update_presentation_memory(pane, memory)?
            }
            WorkspaceLayoutCommand::FocusPane(pane) => self.layout.focus_pane(pane)?,
            WorkspaceLayoutCommand::MovePane { pane, destination } => {
                self.layout.move_pane(pane, destination)?
            }
            WorkspaceLayoutCommand::TearOffPane { pane, placement } => {
                self.layout.tear_off_pane(pane, placement)?
            }
            WorkspaceLayoutCommand::CloseTab(pane) => self.layout.close_tab(pane)?,
            WorkspaceLayoutCommand::ReopenTab(pane) => self.layout.reopen_tab(pane)?,
            WorkspaceLayoutCommand::DockWindow(window) => {
                self.layout.dock_window_on_close(window)?
            }
        };
        complete_window_lifecycle(&before, &self.layout, &mut transition);
        let document = self.layout.export_document()?;
        self.authority_revision = self.authority_revision.wrapping_add(1).max(1);
        let token = WorkspaceActuationToken(self.next_token);
        self.next_token = self.next_token.wrapping_add(1).max(1);
        self.pending = Some(PendingActuation {
            token,
            before,
            accepted: document.clone(),
            transition: transition.clone(),
        });
        Ok(AcceptedWorkspaceCommand {
            token,
            authority_revision: self.authority_revision,
            transition,
            document,
        })
    }

    pub fn complete(
        &mut self,
        token: WorkspaceActuationToken,
    ) -> Result<(), WorkspaceAuthorityError> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(WorkspaceAuthorityError::NoActuationPending)?;
        if pending.token != token {
            return Err(WorkspaceAuthorityError::WrongActuationToken {
                expected: pending.token,
                actual: token,
            });
        }
        self.pending = None;
        Ok(())
    }

    pub fn fail(
        &mut self,
        token: WorkspaceActuationToken,
        failure: WorkspaceNativeFailure,
    ) -> Result<WorkspaceRollback, WorkspaceAuthorityError> {
        let pending = self
            .pending
            .take()
            .ok_or(WorkspaceAuthorityError::NoActuationPending)?;
        if pending.token != token {
            let expected = pending.token;
            self.pending = Some(pending);
            return Err(WorkspaceAuthorityError::WrongActuationToken {
                expected,
                actual: token,
            });
        }

        let applied_bindings = match failure.operation {
            WorkspaceNativeOperation::ApplyBinding => {
                failure.effect_index.min(pending.transition.bindings.len())
            }
            WorkspaceNativeOperation::ApplyWindow => pending.transition.bindings.len(),
            _ => 0,
        };
        let bindings = inverse_bindings(
            &pending.transition.bindings[..applied_bindings],
            &pending.before,
        );
        let windows = reconciliation_windows(
            &pending.accepted,
            pending.before.document(),
            &pending.before,
        );
        self.layout = pending.before;
        self.authority_revision = self.authority_revision.wrapping_add(1).max(1);
        let document = self.layout.export_document()?;
        Ok(WorkspaceRollback {
            failed_token: token,
            authority_revision: self.authority_revision,
            document,
            bindings,
            windows,
            failure,
        })
    }

    /// Accept and actuate one command. Portable state is changed before the
    /// first adapter call; every adapter failure is converted into a durable
    /// rollback plus best-effort native reconciliation diagnostics.
    pub fn execute<A: WorkspaceNativeActuator>(
        &mut self,
        expected_revision: u64,
        command: WorkspaceLayoutCommand,
        actuator: &mut A,
    ) -> Result<AcceptedWorkspaceCommand, WorkspaceExecuteError> {
        let accepted = self.accept(expected_revision, command)?;
        let failure = (|| {
            actuator
                .apply_document(&accepted.document)
                .map_err(|error| WorkspaceNativeFailure {
                    effect_index: 0,
                    operation: WorkspaceNativeOperation::ApplyDocument,
                    message: error.to_string(),
                })?;
            for (index, effect) in accepted.transition.bindings.iter().copied().enumerate() {
                actuator
                    .apply_binding(effect)
                    .map_err(|error| WorkspaceNativeFailure {
                        effect_index: index,
                        operation: WorkspaceNativeOperation::ApplyBinding,
                        message: error.to_string(),
                    })?;
            }
            for (index, effect) in accepted.transition.windows.iter().copied().enumerate() {
                actuator
                    .apply_window(effect)
                    .map_err(|error| WorkspaceNativeFailure {
                        effect_index: index,
                        operation: WorkspaceNativeOperation::ApplyWindow,
                        message: error.to_string(),
                    })?;
            }
            Ok(())
        })();

        if let Err(failure) = failure {
            let rollback = self.fail(accepted.token, failure)?;
            let mut recovery_diagnostics = Vec::new();
            if let Err(error) = actuator.apply_document(&rollback.document) {
                recovery_diagnostics.push(WorkspaceActuationDiagnostic {
                    operation: WorkspaceNativeOperation::RestoreDocument,
                    effect_index: 0,
                    message: error.to_string(),
                });
            }
            for (index, effect) in rollback.bindings.iter().copied().enumerate() {
                if let Err(error) = actuator.apply_binding(effect) {
                    recovery_diagnostics.push(WorkspaceActuationDiagnostic {
                        operation: WorkspaceNativeOperation::RestoreBinding,
                        effect_index: index,
                        message: error.to_string(),
                    });
                }
            }
            for (index, effect) in rollback.windows.iter().copied().enumerate() {
                if let Err(error) = actuator.apply_window(effect) {
                    recovery_diagnostics.push(WorkspaceActuationDiagnostic {
                        operation: WorkspaceNativeOperation::RestoreWindow,
                        effect_index: index,
                        message: error.to_string(),
                    });
                }
            }
            return Err(WorkspaceExecuteError::Native(WorkspaceExecutionFailure {
                rollback,
                recovery_diagnostics,
            }));
        }
        self.complete(accepted.token)?;
        Ok(accepted)
    }
}

fn complete_window_lifecycle(
    before: &WorkspaceSessionLayout,
    after: &WorkspaceSessionLayout,
    transition: &mut WorkspaceLayoutTransition,
) {
    let missing_closes = before
        .document()
        .floating_windows
        .keys()
        .filter(|window| !after.document().floating_windows.contains_key(window))
        .copied()
        .filter(|window| {
            !transition.windows.iter().any(
                |effect| matches!(effect, NativeWindowEffect::Close { window: existing } if existing == window),
            )
        })
        .map(|window| NativeWindowEffect::Close { window })
        .collect::<Vec<_>>();
    for effect in missing_closes.into_iter().rev() {
        transition.windows.insert(0, effect);
    }

    let missing_opens = after
        .document()
        .floating_windows
        .iter()
        .filter(|(window, _)| !before.document().floating_windows.contains_key(window))
        .filter(|(window, _)| {
            !transition.windows.iter().any(
                |effect| matches!(effect, NativeWindowEffect::Open { window: existing, .. } if existing == *window),
            )
        })
        .map(|(&window, descriptor)| NativeWindowEffect::Open {
            window,
            placement: descriptor.placement,
        })
        .collect::<Vec<_>>();
    let first_focus = transition
        .windows
        .iter()
        .position(|effect| matches!(effect, NativeWindowEffect::Focus { .. }))
        .unwrap_or(transition.windows.len());
    transition
        .windows
        .splice(first_focus..first_focus, missing_opens);
}

fn replacement_transition(
    before: &WorkspaceSessionLayout,
    after: &WorkspaceSessionLayout,
    revision: u64,
) -> WorkspaceLayoutTransition {
    let before_visible = before
        .pane_ids()
        .filter(|pane| before.placement(*pane).is_some())
        .collect::<BTreeSet<_>>();
    let after_visible = after
        .pane_ids()
        .filter(|pane| after.placement(*pane).is_some())
        .collect::<BTreeSet<_>>();
    let mut bindings = before_visible
        .difference(&after_visible)
        .copied()
        .map(PaneBindingEffect::Detach)
        .collect::<Vec<_>>();
    let changed_links = before_visible
        .intersection(&after_visible)
        .copied()
        .filter(|pane| {
            before.document().views.get(&pane.0).map(|view| view.links)
                != after.document().views.get(&pane.0).map(|view| view.links)
        })
        .collect::<Vec<_>>();
    bindings.extend(changed_links.iter().copied().map(PaneBindingEffect::Detach));
    bindings.extend(
        after_visible
            .difference(&before_visible)
            .copied()
            .chain(changed_links)
            .filter_map(|pane| {
                after.document().views.get(&pane.0).map(|view| {
                    PaneBindingEffect::Attach(PaneSessionRegistration {
                        view: view.id,
                        links: view.links,
                        topics: PaneSessionTopics::ALL,
                    })
                })
            }),
    );

    let mut windows = before
        .document()
        .floating_windows
        .keys()
        .filter(|window| {
            after
                .document()
                .floating_windows
                .get(window)
                .is_none_or(|next| before.document().floating_windows.get(window) != Some(next))
        })
        .copied()
        .map(|window| NativeWindowEffect::Close { window })
        .collect::<Vec<_>>();
    windows.extend(
        after
            .document()
            .floating_windows
            .iter()
            .filter(|(window, next)| before.document().floating_windows.get(window) != Some(*next))
            .map(|(&window, descriptor)| NativeWindowEffect::Open {
                window,
                placement: descriptor.placement,
            }),
    );
    for window in std::iter::once(WorkspaceWindow::Main).chain(
        after
            .document()
            .floating_windows
            .keys()
            .copied()
            .map(WorkspaceWindow::Floating),
    ) {
        if let Some(pane) = after.focused_pane(window) {
            windows.push(NativeWindowEffect::Focus { window, pane });
        }
    }
    WorkspaceLayoutTransition {
        revision,
        bindings,
        windows,
    }
}

fn inverse_bindings(
    effects: &[PaneBindingEffect],
    before: &WorkspaceSessionLayout,
) -> Vec<PaneBindingEffect> {
    effects
        .iter()
        .rev()
        .filter_map(|effect| match *effect {
            PaneBindingEffect::Attach(registration) => {
                Some(PaneBindingEffect::Detach(PaneInstanceId(registration.view)))
            }
            PaneBindingEffect::Detach(pane) => before.document().views.get(&pane.0).map(|view| {
                PaneBindingEffect::Attach(PaneSessionRegistration {
                    view: view.id,
                    links: view.links,
                    topics: PaneSessionTopics::ALL,
                })
            }),
        })
        .collect()
}

fn reconciliation_windows(
    accepted: &WorkspaceDocument,
    before: &WorkspaceDocument,
    before_layout: &WorkspaceSessionLayout,
) -> Vec<NativeWindowEffect> {
    let mut effects = accepted
        .floating_windows
        .keys()
        .filter(|window| {
            before
                .floating_windows
                .get(window)
                .is_none_or(|previous| accepted.floating_windows.get(window) != Some(previous))
        })
        .copied()
        .map(|window| NativeWindowEffect::Close { window })
        .collect::<Vec<_>>();
    effects.extend(
        before
            .floating_windows
            .iter()
            .filter(|(window, previous)| accepted.floating_windows.get(window) != Some(*previous))
            .map(|(&window, descriptor)| NativeWindowEffect::Open {
                window,
                placement: descriptor.placement,
            }),
    );
    for window in std::iter::once(WorkspaceWindow::Main).chain(
        before
            .floating_windows
            .keys()
            .copied()
            .map(WorkspaceWindow::Floating),
    ) {
        if let Some(pane) = before_layout.focused_pane(window) {
            effects.push(NativeWindowEffect::Focus { window, pane });
        }
    }
    effects
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAuthorityError {
    StaleRevision {
        expected: u64,
        actual: u64,
    },
    ActuationPending(WorkspaceActuationToken),
    NoActuationPending,
    WrongActuationToken {
        expected: WorkspaceActuationToken,
        actual: WorkspaceActuationToken,
    },
    Layout(WorkspaceSessionLayoutError),
}

impl fmt::Display for WorkspaceAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleRevision { expected, actual } => write!(
                formatter,
                "stale workspace command at revision {expected}; current revision is {actual}"
            ),
            Self::ActuationPending(token) => {
                write!(
                    formatter,
                    "workspace native actuation {} is still pending",
                    token.0
                )
            }
            Self::NoActuationPending => {
                formatter.write_str("no workspace native actuation is pending")
            }
            Self::WrongActuationToken { expected, actual } => write!(
                formatter,
                "workspace actuation token {} does not match pending token {}",
                actual.0, expected.0
            ),
            Self::Layout(error) => error.fmt(formatter),
        }
    }
}

impl Error for WorkspaceAuthorityError {}

impl From<WorkspaceSessionLayoutError> for WorkspaceAuthorityError {
    fn from(error: WorkspaceSessionLayoutError) -> Self {
        Self::Layout(error)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceExecuteError {
    Authority(WorkspaceAuthorityError),
    Native(WorkspaceExecutionFailure),
}

impl fmt::Display for WorkspaceExecuteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authority(error) => error.fmt(formatter),
            Self::Native(failure) => write!(
                formatter,
                "workspace native {} failed: {}",
                failure.rollback.failure.operation, failure.rollback.failure.message
            ),
        }
    }
}

impl Error for WorkspaceExecuteError {}

impl From<WorkspaceAuthorityError> for WorkspaceExecuteError {
    fn from(error: WorkspaceAuthorityError) -> Self {
        Self::Authority(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_session::ProjectSessionId;
    use crate::workspace_document::{
        LegacyBuiltinView, WindowMode, WindowPlacement, WorkspaceDocument,
    };
    use crate::workspace_session_layout::{PanePresentationMemory, PaneScrollState};

    fn authority() -> WorkspaceCommandAuthority {
        WorkspaceCommandAuthority::new(
            WorkspaceSessionLayout::from_document(
                ProjectSessionId(41),
                WorkspaceDocument::default(),
            )
            .unwrap(),
        )
    }

    #[derive(Default)]
    struct RecordingActuator {
        documents: Vec<WorkspaceDocument>,
        windows: Vec<NativeWindowEffect>,
        fail_open: bool,
    }

    impl WorkspaceNativeActuator for RecordingActuator {
        type Error = &'static str;

        fn apply_document(&mut self, document: &WorkspaceDocument) -> Result<(), Self::Error> {
            self.documents.push(document.clone());
            Ok(())
        }

        fn apply_binding(&mut self, _effect: PaneBindingEffect) -> Result<(), Self::Error> {
            Ok(())
        }

        fn apply_window(&mut self, effect: NativeWindowEffect) -> Result<(), Self::Error> {
            self.windows.push(effect);
            if self.fail_open && matches!(effect, NativeWindowEffect::Open { .. }) {
                self.fail_open = false;
                return Err("native open refused");
            }
            Ok(())
        }
    }

    #[test]
    fn stale_command_never_mutates_portable_layout() {
        let mut authority = authority();
        let before = authority.export_document().unwrap();
        let error = authority
            .accept(
                99,
                WorkspaceLayoutCommand::FocusPane(PaneInstanceId(
                    LegacyBuiltinView::Waterfall.id(),
                )),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            WorkspaceAuthorityError::StaleRevision { .. }
        ));
        assert_eq!(authority.export_document().unwrap(), before);
        assert!(!authority.has_pending_actuation());
    }

    #[test]
    fn presentation_memory_is_a_typed_serial_command_without_window_churn() {
        let mut authority = authority();
        let pane = PaneInstanceId(LegacyBuiltinView::Rhythm.id());
        let accepted = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::UpdatePresentationMemory {
                    pane,
                    memory: PanePresentationMemory {
                        scroll: PaneScrollState {
                            horizontal: 42.0,
                            vertical: 128.0,
                        },
                        focus_region: Some("result-list".into()),
                        reopen_at: None,
                    },
                },
            )
            .unwrap();
        assert!(accepted.transition.bindings.is_empty());
        assert!(accepted.transition.windows.is_empty());
        authority.complete(accepted.token).unwrap();
        assert_eq!(
            authority.layout().presentation_memory(pane).unwrap().scroll,
            PaneScrollState {
                horizontal: 42.0,
                vertical: 128.0,
            }
        );
    }

    #[test]
    fn native_open_failure_rolls_back_document_and_advances_generation() {
        let mut authority = authority();
        let before = authority.export_document().unwrap();
        let revision = authority.revision();
        let accepted = authority
            .accept(
                revision,
                WorkspaceLayoutCommand::TearOffPane {
                    pane: PaneInstanceId(LegacyBuiltinView::Waterfall.id()),
                    placement: None,
                },
            )
            .unwrap();
        assert_ne!(accepted.document, before);
        let rollback = authority
            .fail(
                accepted.token,
                WorkspaceNativeFailure {
                    effect_index: 0,
                    operation: WorkspaceNativeOperation::ApplyWindow,
                    message: "window server rejected request".into(),
                },
            )
            .unwrap();
        assert_eq!(rollback.document, before);
        assert_eq!(authority.export_document().unwrap(), before);
        assert!(rollback
            .windows
            .iter()
            .any(|effect| matches!(effect, NativeWindowEffect::Close { .. })));
        assert!(rollback.authority_revision > accepted.authority_revision);
        assert!(matches!(
            authority.accept(
                revision,
                WorkspaceLayoutCommand::FocusPane(PaneInstanceId(LegacyBuiltinView::Rhythm.id()))
            ),
            Err(WorkspaceAuthorityError::StaleRevision { .. })
        ));
    }

    #[test]
    fn close_failure_restores_binding_and_stable_view_identity() {
        let mut authority = authority();
        let pane = PaneInstanceId(LegacyBuiltinView::Waterfall.id());
        let accepted = authority
            .accept(authority.revision(), WorkspaceLayoutCommand::CloseTab(pane))
            .unwrap();
        let rollback = authority
            .fail(
                accepted.token,
                WorkspaceNativeFailure {
                    effect_index: 0,
                    operation: WorkspaceNativeOperation::ApplyWindow,
                    message: "pane group rejected snapshot".into(),
                },
            )
            .unwrap();
        assert!(rollback.bindings.iter().any(|effect| matches!(
            effect,
            PaneBindingEffect::Attach(registration) if registration.view == pane.0
        )));
        assert!(authority.document().views.contains_key(&pane.0));
        assert!(authority.layout().placement(pane).is_some());
    }

    #[test]
    fn failed_binding_is_not_inverted_when_it_never_applied() {
        let mut authority = authority();
        let pane = PaneInstanceId(LegacyBuiltinView::Waterfall.id());
        let accepted = authority
            .accept(authority.revision(), WorkspaceLayoutCommand::CloseTab(pane))
            .unwrap();
        let rollback = authority
            .fail(
                accepted.token,
                WorkspaceNativeFailure {
                    effect_index: 0,
                    operation: WorkspaceNativeOperation::ApplyBinding,
                    message: "detach refused".into(),
                },
            )
            .unwrap();
        assert!(rollback.bindings.is_empty());
        assert!(authority.layout().placement(pane).is_some());
    }

    #[test]
    fn acknowledged_roundtrip_preserves_window_and_view_ids() {
        let mut authority = authority();
        let pane = PaneInstanceId(LegacyBuiltinView::Waterfall.id());
        let accepted = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::TearOffPane {
                    pane,
                    placement: None,
                },
            )
            .unwrap();
        let window = accepted
            .document
            .floating_windows
            .keys()
            .next()
            .copied()
            .unwrap();
        authority.complete(accepted.token).unwrap();
        let docked = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::DockWindow(window),
            )
            .unwrap();
        authority.complete(docked.token).unwrap();
        assert_eq!(
            authority.layout().placement(pane).unwrap().window,
            WorkspaceWindow::Main
        );
        assert_eq!(pane.0, LegacyBuiltinView::Waterfall.id());
        assert!(!authority.document().floating_windows.contains_key(&window));
    }

    #[test]
    fn retear_from_single_pane_floater_closes_superseded_native_window() {
        let mut authority = authority();
        let pane = PaneInstanceId(LegacyBuiltinView::Waterfall.id());
        let first = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::TearOffPane {
                    pane,
                    placement: None,
                },
            )
            .unwrap();
        let old_window = first
            .document
            .floating_windows
            .keys()
            .next()
            .copied()
            .unwrap();
        authority.complete(first.token).unwrap();
        let second = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::TearOffPane {
                    pane,
                    placement: None,
                },
            )
            .unwrap();
        assert!(second.transition.windows.iter().any(|effect| matches!(
            effect,
            NativeWindowEffect::Close { window } if *window == old_window
        )));
        assert!(second.transition.windows.iter().any(|effect| matches!(
            effect,
            NativeWindowEffect::Open { window, .. } if *window != old_window
        )));
    }

    #[test]
    fn executor_applies_portable_document_first_and_reconciles_native_failure() {
        let mut authority = authority();
        let before = authority.export_document().unwrap();
        let mut actuator = RecordingActuator {
            fail_open: true,
            ..Default::default()
        };
        let error = authority
            .execute(
                authority.revision(),
                WorkspaceLayoutCommand::TearOffPane {
                    pane: PaneInstanceId(LegacyBuiltinView::Waterfall.id()),
                    placement: None,
                },
                &mut actuator,
            )
            .unwrap_err();
        let WorkspaceExecuteError::Native(failure) = error else {
            panic!("expected native failure")
        };
        assert_eq!(actuator.documents.len(), 2);
        assert_ne!(actuator.documents[0], before);
        assert_eq!(actuator.documents[1], before);
        assert_eq!(failure.rollback.document, before);
        assert!(failure.recovery_diagnostics.is_empty());
        assert!(matches!(
            actuator.windows.first(),
            Some(NativeWindowEffect::Open { .. })
        ));
        assert!(actuator
            .windows
            .iter()
            .skip(1)
            .any(|effect| matches!(effect, NativeWindowEffect::Close { .. })));
    }

    #[test]
    fn document_replacement_derives_visibility_binding_effects() {
        let mut authority = authority();
        let removed = LegacyBuiltinView::Waterfall.id();
        let retained = LegacyBuiltinView::Rhythm.id();
        let mut document = authority.export_document().unwrap();
        document.close_view(removed).unwrap();
        let accepted = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::ReplaceDocument { document },
            )
            .unwrap();
        assert!(accepted.transition.bindings.iter().any(|effect| matches!(
            effect,
            PaneBindingEffect::Detach(pane) if pane.0 == removed
        )));
        assert!(!accepted.document.views.contains_key(&removed));
        assert!(accepted.document.views.contains_key(&retained));
        authority.complete(accepted.token).unwrap();
    }

    #[test]
    fn project_reopen_recreates_same_stable_window_when_placement_changes() {
        let mut authority = authority();
        let first = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::TearOffPane {
                    pane: PaneInstanceId(LegacyBuiltinView::Waterfall.id()),
                    placement: None,
                },
            )
            .unwrap();
        let window = *first.document.floating_windows.keys().next().unwrap();
        authority.complete(first.token).unwrap();
        let mut reopened = authority.export_document().unwrap();
        reopened
            .floating_windows
            .get_mut(&window)
            .unwrap()
            .placement = Some(WindowPlacement {
            mode: WindowMode::Windowed,
            x: 80.0,
            y: 40.0,
            width: 900.0,
            height: 600.0,
        });
        let accepted = authority
            .accept(
                authority.revision(),
                WorkspaceLayoutCommand::ReplaceDocument { document: reopened },
            )
            .unwrap();
        assert!(accepted.transition.windows.iter().any(|effect| matches!(
            effect,
            NativeWindowEffect::Close { window: actual } if *actual == window
        )));
        assert!(accepted.transition.windows.iter().any(|effect| matches!(
            effect,
            NativeWindowEffect::Open { window: actual, placement: Some(_) } if *actual == window
        )));
    }
}
