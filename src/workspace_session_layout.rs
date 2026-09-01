//! Authoritative, toolkit-neutral workspace/session placement state.
//!
//! [`WorkspaceSessionLayout`] wraps the existing durable [`WorkspaceDocument`]
//! rather than inventing another dock format. A pane instance is exactly one
//! [`WorkspaceViewId`]; moving it between dock trees or native windows never
//! allocates a replacement identity, reconstructs an editor, or touches the
//! project/audio transport. The wrapper adds transition semantics that the
//! portable document deliberately does not own: focus, close/reopen anchors,
//! scroll memory, and explicit attach/detach effects for [`PaneSessionBinding`].
//!
//! GPUI entities, Guise IDs, native window handles, and pixels measured from a
//! live window are intentionally absent. The application adapter applies a
//! committed transition's effects after updating its entity/window maps.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::pane_session_binding::{
    PaneSessionBinding, PaneSessionBindingError, PaneSessionDelivery, PaneSessionRegistration,
    PaneSessionTopics,
};
use crate::project_session::{ProjectSession, ProjectSessionId};
use crate::workspace_document::{
    CloseBehavior, DockLayout, DockPaneId, EditorViewState, ViewLocation, WindowPlacement,
    WorkspaceDocument, WorkspaceDocumentError, WorkspaceViewId, WorkspaceWindowId,
};

const SESSION_LAYOUT_EXTENSION: &str = "audec.workspace-session-layout.v1";

/// A named pane instance is the durable workspace view identity itself. This
/// newtype is a semantic API boundary, not a separately allocated ID domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneInstanceId(pub WorkspaceViewId);

impl From<WorkspaceViewId> for PaneInstanceId {
    fn from(value: WorkspaceViewId) -> Self {
        Self(value)
    }
}

impl From<PaneInstanceId> for WorkspaceViewId {
    fn from(value: PaneInstanceId) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum WorkspaceWindow {
    Main,
    Floating(WorkspaceWindowId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePlacement {
    pub window: WorkspaceWindow,
    pub dock_pane: DockPaneId,
    pub tab_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneMoveDestination {
    pub window: WorkspaceWindow,
    pub dock_pane: DockPaneId,
    pub tab_index: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneScrollState {
    pub horizontal: f32,
    pub vertical: f32,
}

impl PaneScrollState {
    fn validate(self) -> Result<(), WorkspaceSessionLayoutError> {
        if !self.horizontal.is_finite()
            || !self.vertical.is_finite()
            || self.horizontal < 0.0
            || self.vertical < 0.0
        {
            return Err(WorkspaceSessionLayoutError::InvalidScrollState);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PanePresentationMemory {
    #[serde(default)]
    pub scroll: PaneScrollState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reopen_at: Option<PanePlacement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WindowFocusRecord {
    window: WorkspaceWindow,
    pane: PaneInstanceId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct PaneMemoryRecord {
    pane: PaneInstanceId,
    memory: PanePresentationMemory,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct DurableSessionLayoutMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    focus: Vec<WindowFocusRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    panes: Vec<PaneMemoryRecord>,
}

/// Binding changes are explicit so moving a live entity between windows does
/// not accidentally detach it from the one project transport. Only visibility
/// lifecycle changes produce these effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneBindingEffect {
    Attach(PaneSessionRegistration),
    Detach(PaneInstanceId),
}

impl PaneBindingEffect {
    pub fn apply(
        self,
        binding: &mut PaneSessionBinding,
        session: &mut ProjectSession,
    ) -> Result<Option<PaneSessionDelivery>, PaneSessionBindingError> {
        match self {
            Self::Attach(registration) => binding.register_pane(session, registration).map(Some),
            Self::Detach(pane) => {
                binding.unregister_pane(session, pane.0);
                Ok(None)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeWindowEffect {
    Open {
        window: WorkspaceWindowId,
        placement: Option<WindowPlacement>,
    },
    Close {
        window: WorkspaceWindowId,
    },
    Focus {
        window: WorkspaceWindow,
        pane: PaneInstanceId,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkspaceLayoutTransition {
    pub revision: u64,
    pub bindings: Vec<PaneBindingEffect>,
    pub windows: Vec<NativeWindowEffect>,
}

/// One workspace presentation attached to exactly one runtime project session.
#[derive(Clone, Debug)]
pub struct WorkspaceSessionLayout {
    session: ProjectSessionId,
    document: WorkspaceDocument,
    focused: BTreeMap<WorkspaceWindow, PaneInstanceId>,
    memory: BTreeMap<PaneInstanceId, PanePresentationMemory>,
    revision: u64,
}

impl WorkspaceSessionLayout {
    pub fn from_document(
        session: ProjectSessionId,
        document: WorkspaceDocument,
    ) -> Result<Self, WorkspaceSessionLayoutError> {
        if session.0 == 0 {
            return Err(WorkspaceSessionLayoutError::ZeroSession);
        }
        document.validate()?;
        let metadata = match document.extensions.get(SESSION_LAYOUT_EXTENSION) {
            Some(value) => serde_json::from_value::<DurableSessionLayoutMetadata>(value.clone())
                .map_err(|error| WorkspaceSessionLayoutError::Metadata(error.to_string()))?,
            None => DurableSessionLayoutMetadata::default(),
        };
        let mut focused = BTreeMap::new();
        for record in metadata.focus {
            if document.views.contains_key(&record.pane.0)
                && placement_of(&document, record.pane)
                    .is_some_and(|placement| placement.window == record.window)
            {
                focused.insert(record.window, record.pane);
            }
        }
        let mut memory = BTreeMap::new();
        for record in metadata.panes {
            if document.views.contains_key(&record.pane.0) {
                record.memory.scroll.validate()?;
                memory.insert(record.pane, record.memory);
            }
        }
        let mut layout = Self {
            session,
            document,
            focused,
            memory,
            revision: 0,
        };
        layout.ensure_window_focuses();
        Ok(layout)
    }

    pub const fn session_id(&self) -> ProjectSessionId {
        self.session
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn document(&self) -> &WorkspaceDocument {
        &self.document
    }

    pub fn export_document(&self) -> Result<WorkspaceDocument, WorkspaceSessionLayoutError> {
        let mut document = self.document.clone();
        let metadata = DurableSessionLayoutMetadata {
            focus: self
                .focused
                .iter()
                .map(|(&window, &pane)| WindowFocusRecord { window, pane })
                .collect(),
            panes: self
                .memory
                .iter()
                .map(|(&pane, memory)| PaneMemoryRecord {
                    pane,
                    memory: memory.clone(),
                })
                .collect(),
        };
        let value = serde_json::to_value(metadata)
            .map_err(|error| WorkspaceSessionLayoutError::Metadata(error.to_string()))?;
        document
            .extensions
            .insert(SESSION_LAYOUT_EXTENSION.into(), value);
        document.validate()?;
        Ok(document)
    }

    pub fn pane_ids(&self) -> impl ExactSizeIterator<Item = PaneInstanceId> + '_ {
        self.document.views.keys().copied().map(PaneInstanceId)
    }

    pub fn placement(&self, pane: PaneInstanceId) -> Option<PanePlacement> {
        placement_of(&self.document, pane)
    }

    pub fn focused_pane(&self, window: WorkspaceWindow) -> Option<PaneInstanceId> {
        self.focused.get(&window).copied()
    }

    pub fn presentation_memory(&self, pane: PaneInstanceId) -> Option<&PanePresentationMemory> {
        self.memory.get(&pane)
    }

    /// Register every currently visible pane with the one project session.
    /// Hidden tabs attach only when reopened.
    pub fn initial_binding_effects(&self) -> Vec<PaneBindingEffect> {
        self.document
            .views
            .values()
            .filter(|descriptor| {
                !matches!(
                    self.document.location(descriptor.id),
                    Ok(ViewLocation::Hidden)
                )
            })
            .map(|descriptor| {
                PaneBindingEffect::Attach(PaneSessionRegistration {
                    view: descriptor.id,
                    links: descriptor.links,
                    topics: PaneSessionTopics::ALL,
                })
            })
            .collect()
    }

    pub fn update_view_state(
        &mut self,
        pane: PaneInstanceId,
        state: EditorViewState,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        let mut descriptor = self
            .document
            .views
            .get(&pane.0)
            .cloned()
            .ok_or(WorkspaceSessionLayoutError::UnknownPane(pane))?;
        descriptor.state = state;
        self.document.replace_view(descriptor)?;
        Ok(self.finish_transition(Vec::new(), Vec::new()))
    }

    /// Replace a native window's dock tree after translating a Guise snapshot
    /// back to durable workspace identities. This is the adapter seam for
    /// split creation/removal and divider movement; cross-window tab movement
    /// should still use [`Self::move_pane`] so it remains one atomic semantic
    /// operation.
    pub fn replace_window_layout(
        &mut self,
        window: WorkspaceWindow,
        layout: DockLayout,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        match window {
            WorkspaceWindow::Main => self.document.replace_main_layout(layout)?,
            WorkspaceWindow::Floating(window) => {
                self.document.replace_floating_layout(window, layout)?
            }
        }
        self.ensure_window_focuses();
        Ok(self.finish_transition(Vec::new(), Vec::new()))
    }

    /// Persist the last known logical window geometry without making a native
    /// handle part of the portable workspace document.
    pub fn set_window_placement(
        &mut self,
        window: WorkspaceWindow,
        placement: Option<WindowPlacement>,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        match window {
            WorkspaceWindow::Main => self.document.set_main_window(placement)?,
            WorkspaceWindow::Floating(window) => self
                .document
                .set_floating_window_placement(window, placement)?,
        }
        Ok(self.finish_transition(Vec::new(), Vec::new()))
    }

    pub fn update_presentation_memory(
        &mut self,
        pane: PaneInstanceId,
        mut memory: PanePresentationMemory,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        if !self.document.views.contains_key(&pane.0) {
            return Err(WorkspaceSessionLayoutError::UnknownPane(pane));
        }
        memory.scroll.validate()?;
        if memory
            .focus_region
            .as_ref()
            .is_some_and(|region| region.trim().is_empty())
        {
            memory.focus_region = None;
        }
        self.memory.insert(pane, memory);
        Ok(self.finish_transition(Vec::new(), Vec::new()))
    }

    pub fn focus_pane(
        &mut self,
        pane: PaneInstanceId,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        let placement = self
            .placement(pane)
            .ok_or(WorkspaceSessionLayoutError::PaneHidden(pane))?;
        activate_tab(self.layout_mut(placement.window)?, pane.0)?;
        self.focused.insert(placement.window, pane);
        Ok(self.finish_transition(
            Vec::new(),
            vec![NativeWindowEffect::Focus {
                window: placement.window,
                pane,
            }],
        ))
    }

    /// Move a visible pane between existing dock panes/windows. The session
    /// binding remains attached because the same editor entity remains live.
    pub fn move_pane(
        &mut self,
        pane: PaneInstanceId,
        destination: PaneMoveDestination,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        let source = self
            .placement(pane)
            .ok_or(WorkspaceSessionLayoutError::PaneHidden(pane))?;
        if matches!(destination.window, WorkspaceWindow::Floating(_))
            && !self
                .document
                .views
                .get(&pane.0)
                .ok_or(WorkspaceSessionLayoutError::UnknownPane(pane))?
                .kind
                .can_float()
        {
            return Err(WorkspaceSessionLayoutError::PaneCannotFloat(pane));
        }

        let mut next = self.clone();
        if source.window == destination.window && source.dock_pane == destination.dock_pane {
            reorder_tab(
                next.layout_mut(source.window)?,
                pane.0,
                destination.tab_index,
            )?;
        } else {
            next.remove_visible_pane(pane)?;
            insert_into_pane(
                next.layout_mut(destination.window)?,
                destination.dock_pane,
                pane.0,
                destination.tab_index,
            )?;
        }
        next.document.validate()?;
        next.focused.insert(destination.window, pane);
        next.ensure_window_focuses();
        next.revision = self.revision.wrapping_add(1).max(1);

        let mut windows = vec![NativeWindowEffect::Focus {
            window: destination.window,
            pane,
        }];
        if let WorkspaceWindow::Floating(source_window) = source.window {
            if !next.document.floating_windows.contains_key(&source_window) {
                windows.insert(
                    0,
                    NativeWindowEffect::Close {
                        window: source_window,
                    },
                );
            }
        }
        *self = next;
        Ok(WorkspaceLayoutTransition {
            revision: self.revision,
            bindings: Vec::new(),
            windows,
        })
    }

    /// Tear one visible pane into a new native window without changing its
    /// binding or editor identity.
    pub fn tear_off_pane(
        &mut self,
        pane: PaneInstanceId,
        placement: Option<WindowPlacement>,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        if self.placement(pane).is_none() {
            return Err(WorkspaceSessionLayoutError::PaneHidden(pane));
        }
        let window = self.document.tear_off_view(pane.0, placement)?;
        self.focused.retain(|_, focused| *focused != pane);
        self.focused.insert(WorkspaceWindow::Floating(window), pane);
        self.ensure_window_focuses();
        Ok(self.finish_transition(
            Vec::new(),
            vec![
                NativeWindowEffect::Open { window, placement },
                NativeWindowEffect::Focus {
                    window: WorkspaceWindow::Floating(window),
                    pane,
                },
            ],
        ))
    }

    /// Close a tab while retaining its descriptor, stable identity, view
    /// state, and last placement. Permanent descriptor destruction is a
    /// separate workspace-management action, never an ordinary tab close.
    pub fn close_tab(
        &mut self,
        pane: PaneInstanceId,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        let descriptor = self
            .document
            .views
            .get(&pane.0)
            .ok_or(WorkspaceSessionLayoutError::UnknownPane(pane))?;
        if descriptor.kind.close_behavior() == CloseBehavior::Pinned {
            return Err(WorkspaceSessionLayoutError::PinnedPane(pane));
        }
        let placement = self
            .placement(pane)
            .ok_or(WorkspaceSessionLayoutError::PaneAlreadyHidden(pane))?;
        let mut next = self.clone();
        next.remove_visible_pane(pane)?;
        next.memory.entry(pane).or_default().reopen_at = Some(placement);
        next.focused.retain(|_, focused| *focused != pane);
        next.ensure_window_focuses();
        next.document.validate()?;
        next.revision = self.revision.wrapping_add(1).max(1);

        let mut windows = Vec::new();
        if let WorkspaceWindow::Floating(window) = placement.window {
            if !next.document.floating_windows.contains_key(&window) {
                windows.push(NativeWindowEffect::Close { window });
            }
        }
        *self = next;
        Ok(WorkspaceLayoutTransition {
            revision: self.revision,
            bindings: vec![PaneBindingEffect::Detach(pane)],
            windows,
        })
    }

    pub fn reopen_tab(
        &mut self,
        pane: PaneInstanceId,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        if !self.document.views.contains_key(&pane.0) {
            return Err(WorkspaceSessionLayoutError::UnknownPane(pane));
        }
        if self.placement(pane).is_some() {
            return self.focus_pane(pane);
        }
        let remembered = self.memory.get(&pane).and_then(|memory| memory.reopen_at);
        let destination = remembered
            .filter(|placement| self.has_dock_pane(placement.window, placement.dock_pane))
            .unwrap_or(PanePlacement {
                window: WorkspaceWindow::Main,
                dock_pane: self.document.main_layout.primary_pane(),
                tab_index: usize::MAX,
            });
        insert_into_pane(
            self.layout_mut(destination.window)?,
            destination.dock_pane,
            pane.0,
            destination.tab_index,
        )?;
        self.focused.insert(destination.window, pane);
        self.document.validate()?;
        let descriptor = self
            .document
            .views
            .get(&pane.0)
            .expect("reopened descriptor checked above");
        Ok(self.finish_transition(
            vec![PaneBindingEffect::Attach(PaneSessionRegistration {
                view: pane.0,
                links: descriptor.links,
                topics: PaneSessionTopics::ALL,
            })],
            vec![NativeWindowEffect::Focus {
                window: destination.window,
                pane,
            }],
        ))
    }

    /// Native-window close defaults to docking every contained pane back into
    /// the main workspace. Bindings stay attached throughout.
    pub fn dock_window_on_close(
        &mut self,
        window: WorkspaceWindowId,
    ) -> Result<WorkspaceLayoutTransition, WorkspaceSessionLayoutError> {
        let panes = self
            .document
            .floating_windows
            .get(&window)
            .ok_or(WorkspaceSessionLayoutError::UnknownWindow(window))
            .map(|floating| collect_items(&floating.layout))?;
        self.document.dock_window(window)?;
        self.document.validate()?;
        self.focused.remove(&WorkspaceWindow::Floating(window));
        if let Some(&pane) = panes.last() {
            self.focused
                .insert(WorkspaceWindow::Main, PaneInstanceId(pane));
        }
        self.ensure_window_focuses();
        let mut windows = vec![NativeWindowEffect::Close { window }];
        if let Some(&pane) = panes.last() {
            windows.push(NativeWindowEffect::Focus {
                window: WorkspaceWindow::Main,
                pane: PaneInstanceId(pane),
            });
        }
        Ok(self.finish_transition(Vec::new(), windows))
    }

    fn has_dock_pane(&self, window: WorkspaceWindow, pane: DockPaneId) -> bool {
        self.layout(window)
            .is_ok_and(|layout| find_pane(layout, pane).is_some())
    }

    fn layout(&self, window: WorkspaceWindow) -> Result<&DockLayout, WorkspaceSessionLayoutError> {
        match window {
            WorkspaceWindow::Main => Ok(&self.document.main_layout),
            WorkspaceWindow::Floating(window) => self
                .document
                .floating_windows
                .get(&window)
                .map(|window| &window.layout)
                .ok_or(WorkspaceSessionLayoutError::UnknownWindow(window)),
        }
    }

    fn layout_mut(
        &mut self,
        window: WorkspaceWindow,
    ) -> Result<&mut DockLayout, WorkspaceSessionLayoutError> {
        match window {
            WorkspaceWindow::Main => Ok(&mut self.document.main_layout),
            WorkspaceWindow::Floating(window) => self
                .document
                .floating_windows
                .get_mut(&window)
                .map(|window| &mut window.layout)
                .ok_or(WorkspaceSessionLayoutError::UnknownWindow(window)),
        }
    }

    fn remove_visible_pane(
        &mut self,
        pane: PaneInstanceId,
    ) -> Result<(), WorkspaceSessionLayoutError> {
        let placement = self
            .placement(pane)
            .ok_or(WorkspaceSessionLayoutError::PaneHidden(pane))?;
        match placement.window {
            WorkspaceWindow::Main => {
                let (layout, removed) =
                    remove_from_layout(self.document.main_layout.clone(), pane.0);
                debug_assert!(removed);
                self.document.main_layout =
                    layout.ok_or(WorkspaceSessionLayoutError::CannotEmptyMainWindow)?;
            }
            WorkspaceWindow::Floating(window) => {
                let floating = self
                    .document
                    .floating_windows
                    .get(&window)
                    .ok_or(WorkspaceSessionLayoutError::UnknownWindow(window))?;
                let (layout, removed) = remove_from_layout(floating.layout.clone(), pane.0);
                debug_assert!(removed);
                if let Some(layout) = layout {
                    self.document
                        .floating_windows
                        .get_mut(&window)
                        .expect("floating window checked above")
                        .layout = layout;
                } else {
                    self.document.floating_windows.remove(&window);
                    self.focused.remove(&WorkspaceWindow::Floating(window));
                }
            }
        }
        Ok(())
    }

    fn ensure_window_focuses(&mut self) {
        let windows = std::iter::once(WorkspaceWindow::Main)
            .chain(
                self.document
                    .floating_windows
                    .keys()
                    .copied()
                    .map(WorkspaceWindow::Floating),
            )
            .collect::<Vec<_>>();
        self.focused.retain(|window, pane| {
            windows.contains(window)
                && placement_of(&self.document, *pane)
                    .is_some_and(|placement| placement.window == *window)
        });
        for window in windows {
            if !self.focused.contains_key(&window) {
                if let Ok(layout) = self.layout(window) {
                    if let Some(view) = active_item(layout) {
                        self.focused.insert(window, PaneInstanceId(view));
                    }
                }
            }
        }
    }

    fn finish_transition(
        &mut self,
        bindings: Vec<PaneBindingEffect>,
        windows: Vec<NativeWindowEffect>,
    ) -> WorkspaceLayoutTransition {
        self.revision = self.revision.wrapping_add(1).max(1);
        WorkspaceLayoutTransition {
            revision: self.revision,
            bindings,
            windows,
        }
    }
}

fn placement_of(document: &WorkspaceDocument, pane: PaneInstanceId) -> Option<PanePlacement> {
    find_placement(&document.main_layout, pane.0)
        .map(|(dock_pane, tab_index)| PanePlacement {
            window: WorkspaceWindow::Main,
            dock_pane,
            tab_index,
        })
        .or_else(|| {
            document
                .floating_windows
                .iter()
                .find_map(|(&window, floating)| {
                    find_placement(&floating.layout, pane.0).map(|(dock_pane, tab_index)| {
                        PanePlacement {
                            window: WorkspaceWindow::Floating(window),
                            dock_pane,
                            tab_index,
                        }
                    })
                })
        })
}

fn find_placement(layout: &DockLayout, view: WorkspaceViewId) -> Option<(DockPaneId, usize)> {
    match layout {
        DockLayout::Pane { pane_id, items, .. } => items
            .iter()
            .position(|item| *item == view)
            .map(|index| (*pane_id, index)),
        DockLayout::Split { first, second, .. } => {
            find_placement(first, view).or_else(|| find_placement(second, view))
        }
    }
}

fn find_pane(layout: &DockLayout, target: DockPaneId) -> Option<&DockLayout> {
    match layout {
        DockLayout::Pane { pane_id, .. } if *pane_id == target => Some(layout),
        DockLayout::Pane { .. } => None,
        DockLayout::Split { first, second, .. } => {
            find_pane(first, target).or_else(|| find_pane(second, target))
        }
    }
}

fn insert_into_pane(
    layout: &mut DockLayout,
    target: DockPaneId,
    view: WorkspaceViewId,
    tab_index: usize,
) -> Result<(), WorkspaceSessionLayoutError> {
    match layout {
        DockLayout::Pane {
            pane_id,
            items,
            active,
            ..
        } if *pane_id == target => {
            let index = tab_index.min(items.len());
            items.insert(index, view);
            *active = index;
            Ok(())
        }
        DockLayout::Pane { .. } => Err(WorkspaceSessionLayoutError::UnknownDockPane(target)),
        DockLayout::Split { first, second, .. } => {
            if find_pane(first, target).is_some() {
                insert_into_pane(first, target, view, tab_index)
            } else if find_pane(second, target).is_some() {
                insert_into_pane(second, target, view, tab_index)
            } else {
                Err(WorkspaceSessionLayoutError::UnknownDockPane(target))
            }
        }
    }
}

fn reorder_tab(
    layout: &mut DockLayout,
    view: WorkspaceViewId,
    tab_index: usize,
) -> Result<(), WorkspaceSessionLayoutError> {
    match layout {
        DockLayout::Pane { items, active, .. } if items.contains(&view) => {
            let old = items
                .iter()
                .position(|item| *item == view)
                .expect("pane contains view");
            items.remove(old);
            let index = tab_index.min(items.len());
            items.insert(index, view);
            *active = index;
            Ok(())
        }
        DockLayout::Pane { .. } => Err(WorkspaceSessionLayoutError::UnknownPane(PaneInstanceId(
            view,
        ))),
        DockLayout::Split { first, second, .. } => {
            if find_placement(first, view).is_some() {
                reorder_tab(first, view, tab_index)
            } else {
                reorder_tab(second, view, tab_index)
            }
        }
    }
}

fn activate_tab(
    layout: &mut DockLayout,
    view: WorkspaceViewId,
) -> Result<(), WorkspaceSessionLayoutError> {
    match layout {
        DockLayout::Pane { items, active, .. } => {
            if let Some(index) = items.iter().position(|item| *item == view) {
                *active = index;
                Ok(())
            } else {
                Err(WorkspaceSessionLayoutError::UnknownPane(PaneInstanceId(
                    view,
                )))
            }
        }
        DockLayout::Split { first, second, .. } => {
            if find_placement(first, view).is_some() {
                activate_tab(first, view)
            } else {
                activate_tab(second, view)
            }
        }
    }
}

fn active_item(layout: &DockLayout) -> Option<WorkspaceViewId> {
    match layout {
        DockLayout::Pane { items, active, .. } => items.get(*active).copied(),
        DockLayout::Split { first, .. } => active_item(first),
    }
}

fn collect_items(layout: &DockLayout) -> Vec<WorkspaceViewId> {
    let mut items = Vec::new();
    match layout {
        DockLayout::Pane { items: pane, .. } => items.extend(pane),
        DockLayout::Split { first, second, .. } => {
            items.extend(collect_items(first));
            items.extend(collect_items(second));
        }
    }
    items
}

fn remove_from_layout(layout: DockLayout, view: WorkspaceViewId) -> (Option<DockLayout>, bool) {
    match layout {
        DockLayout::Pane {
            pane_id,
            mut items,
            mut active,
            extensions,
        } => {
            let Some(index) = items.iter().position(|item| *item == view) else {
                return (
                    Some(DockLayout::Pane {
                        pane_id,
                        items,
                        active,
                        extensions,
                    }),
                    false,
                );
            };
            items.remove(index);
            if items.is_empty() {
                return (None, true);
            }
            if active >= items.len() {
                active = items.len() - 1;
            } else if index < active {
                active -= 1;
            }
            (
                Some(DockLayout::Pane {
                    pane_id,
                    items,
                    active,
                    extensions,
                }),
                true,
            )
        }
        DockLayout::Split {
            axis,
            ratio,
            first,
            second,
            extensions,
        } => {
            let (first, first_removed) = remove_from_layout(*first, view);
            let (second, second_removed) = remove_from_layout(*second, view);
            let next = match (first, second) {
                (Some(first), Some(second)) => Some(DockLayout::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                    extensions,
                }),
                (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            };
            (next, first_removed || second_removed)
        }
    }
}

/// Platform-independent input to the native-titlebar safety calculation.
/// Coordinates are logical pixels in the window content coordinate system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl LogicalRect {
    fn max_x(self) -> f32 {
        self.x + self.width
    }

    fn validate(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f32::is_finite)
            && self.width >= 0.0
            && self.height >= 0.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContentInsets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowPlatform {
    MacOs,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitlebarComposition {
    /// Tabs occupy the transparent native titlebar; traffic lights and custom
    /// controls are protected by PaneGroup leading/trailing insets.
    OverlayTabs,
    /// Application content begins below the native titlebar.
    ContentBelowTitlebar,
}

pub const WORKSPACE_TITLEBAR_HEIGHT: f32 = 38.0;
pub const WORKSPACE_TITLEBAR_CLEARANCE: f32 = 12.0;

/// Audec's shared titlebar policy for both Guise tab strips and custom chrome.
/// Keeping this at the workspace boundary prevents individual rails/panes from
/// each inventing an 80-ish pixel macOS padding value.
pub fn default_workspace_titlebar_layout(
    platform: WindowPlatform,
    composition: TitlebarComposition,
    traffic_lights: Option<LogicalRect>,
) -> Result<TitlebarSafeLayout, WorkspaceSessionLayoutError> {
    resolve_titlebar_layout(TitlebarLayoutInput {
        platform,
        composition,
        titlebar_height: WORKSPACE_TITLEBAR_HEIGHT,
        traffic_lights,
        custom_leading_width: 0.0,
        custom_trailing_width: 0.0,
        clearance: if platform == WindowPlatform::MacOs {
            WORKSPACE_TITLEBAR_CLEARANCE
        } else {
            0.0
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TitlebarLayoutInput {
    pub platform: WindowPlatform,
    pub composition: TitlebarComposition,
    pub titlebar_height: f32,
    pub traffic_lights: Option<LogicalRect>,
    pub custom_leading_width: f32,
    pub custom_trailing_width: f32,
    pub clearance: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TitlebarSafeLayout {
    pub content: ContentInsets,
    pub pane_group_leading: f32,
    pub pane_group_trailing: f32,
    pub draggable_height: f32,
}

impl TitlebarSafeLayout {
    /// Values passed directly to Guise `PaneGroup::titlebar(leading, trailing)`.
    pub const fn guise_titlebar_insets(self) -> (f32, f32) {
        (self.pane_group_leading, self.pane_group_trailing)
    }
}

/// Resolve safe macOS traffic-light and titlebar geometry without embedding a
/// magic 80 px constant in a renderer. When GPUI cannot report the traffic
/// light rectangle, the fallback trailing edge is 70 logical pixels and the
/// caller-supplied clearance is added.
pub fn resolve_titlebar_layout(
    input: TitlebarLayoutInput,
) -> Result<TitlebarSafeLayout, WorkspaceSessionLayoutError> {
    if ![
        input.titlebar_height,
        input.custom_leading_width,
        input.custom_trailing_width,
        input.clearance,
    ]
    .into_iter()
    .all(|value| value.is_finite() && value >= 0.0)
        || input.traffic_lights.is_some_and(|rect| !rect.validate())
    {
        return Err(WorkspaceSessionLayoutError::InvalidTitlebarMetrics);
    }
    let traffic_trailing = if input.platform == WindowPlatform::MacOs {
        input.traffic_lights.map(LogicalRect::max_x).unwrap_or(70.0)
    } else {
        0.0
    };
    let overlay = input.composition == TitlebarComposition::OverlayTabs;
    Ok(TitlebarSafeLayout {
        content: ContentInsets {
            top: if overlay { 0.0 } else { input.titlebar_height },
            ..ContentInsets::default()
        },
        pane_group_leading: if overlay {
            input
                .custom_leading_width
                .max(traffic_trailing + input.clearance)
        } else {
            input.custom_leading_width
        },
        pane_group_trailing: if overlay {
            input.custom_trailing_width + input.clearance
        } else {
            input.custom_trailing_width
        },
        draggable_height: if overlay { input.titlebar_height } else { 0.0 },
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceSessionLayoutError {
    ZeroSession,
    UnknownPane(PaneInstanceId),
    PaneHidden(PaneInstanceId),
    PaneAlreadyHidden(PaneInstanceId),
    PinnedPane(PaneInstanceId),
    PaneCannotFloat(PaneInstanceId),
    UnknownWindow(WorkspaceWindowId),
    UnknownDockPane(DockPaneId),
    CannotEmptyMainWindow,
    InvalidScrollState,
    InvalidTitlebarMetrics,
    Metadata(String),
    Document(WorkspaceDocumentError),
}

impl fmt::Display for WorkspaceSessionLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSession => formatter.write_str("workspace session ID zero is reserved"),
            Self::UnknownPane(pane) => write!(formatter, "workspace pane {} is unknown", pane.0 .0),
            Self::PaneHidden(pane) => write!(formatter, "workspace pane {} is hidden", pane.0 .0),
            Self::PaneAlreadyHidden(pane) => {
                write!(formatter, "workspace pane {} is already hidden", pane.0 .0)
            }
            Self::PinnedPane(pane) => write!(formatter, "workspace pane {} is pinned", pane.0 .0),
            Self::PaneCannotFloat(pane) => {
                write!(formatter, "workspace pane {} cannot float", pane.0 .0)
            }
            Self::UnknownWindow(window) => {
                write!(formatter, "workspace window {} is unknown", window.0)
            }
            Self::UnknownDockPane(pane) => write!(formatter, "dock pane {} is unknown", pane.0),
            Self::CannotEmptyMainWindow => {
                formatter.write_str("the main workspace cannot be empty")
            }
            Self::InvalidScrollState => {
                formatter.write_str("pane scroll offsets must be finite and non-negative")
            }
            Self::InvalidTitlebarMetrics => {
                formatter.write_str("titlebar metrics must be finite and non-negative")
            }
            Self::Metadata(message) => write!(formatter, "workspace session metadata: {message}"),
            Self::Document(error) => error.fmt(formatter),
        }
    }
}

impl Error for WorkspaceSessionLayoutError {}

impl From<WorkspaceDocumentError> for WorkspaceSessionLayoutError {
    fn from(error: WorkspaceDocumentError) -> Self {
        Self::Document(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{Aspect, FrameSpan, SignalLayer};
    use crate::project_selection::ProjectSelection;
    use crate::project_session::ProjectSessionId;
    use crate::workspace_document::{FrameViewport, LegacyBuiltinView, WindowMode};

    fn layout() -> WorkspaceSessionLayout {
        WorkspaceSessionLayout::from_document(ProjectSessionId(9), WorkspaceDocument::default())
            .unwrap()
    }

    fn pane(view: LegacyBuiltinView) -> PaneInstanceId {
        PaneInstanceId(view.id())
    }

    #[test]
    fn move_between_windows_keeps_identity_view_scroll_and_session_binding() {
        let mut layout = layout();
        let waterfall = pane(LegacyBuiltinView::Waterfall);
        let rhythm = pane(LegacyBuiltinView::Rhythm);
        layout
            .update_view_state(
                rhythm,
                EditorViewState::Analysis {
                    viewport: FrameViewport {
                        start: 4_000,
                        end: 9_000,
                    },
                    follow: true,
                    min_frequency_hz: Some(42.0),
                    max_frequency_hz: Some(12_000.0),
                    recipe_fingerprint: Some("cqt-9".into()),
                },
            )
            .unwrap();
        layout
            .update_presentation_memory(
                rhythm,
                PanePresentationMemory {
                    scroll: PaneScrollState {
                        horizontal: 88.0,
                        vertical: 13.0,
                    },
                    focus_region: Some("canvas".into()),
                    reopen_at: None,
                },
            )
            .unwrap();
        let floated = layout.tear_off_pane(waterfall, None).unwrap();
        let window = floated
            .windows
            .iter()
            .find_map(|effect| match effect {
                NativeWindowEffect::Open { window, .. } => Some(*window),
                _ => None,
            })
            .unwrap();
        let target = layout.placement(waterfall).unwrap();
        let moved = layout
            .move_pane(
                rhythm,
                PaneMoveDestination {
                    window: WorkspaceWindow::Floating(window),
                    dock_pane: target.dock_pane,
                    tab_index: 1,
                },
            )
            .unwrap();
        assert!(
            moved.bindings.is_empty(),
            "window moves must not rebind audio"
        );
        assert_eq!(layout.session_id(), ProjectSessionId(9));
        assert_eq!(
            layout.placement(rhythm).unwrap().window,
            WorkspaceWindow::Floating(window)
        );
        assert_eq!(
            layout.presentation_memory(rhythm).unwrap().scroll,
            PaneScrollState {
                horizontal: 88.0,
                vertical: 13.0,
            }
        );
        assert!(matches!(
            layout.document().views[&rhythm.0].state,
            EditorViewState::Analysis {
                viewport: FrameViewport {
                    start: 4_000,
                    end: 9_000
                },
                follow: true,
                ..
            }
        ));
    }

    #[test]
    fn close_reopen_keeps_stable_instance_and_safely_detaches_binding() {
        let mut layout = layout();
        let rhythm = pane(LegacyBuiltinView::Rhythm);
        let mut session = ProjectSession::new(ProjectSessionId(9)).unwrap();
        let mut binding = PaneSessionBinding::new();
        for effect in layout.initial_binding_effects() {
            effect.apply(&mut binding, &mut session).unwrap();
        }
        assert!(binding.contains(rhythm.0));

        let closed = layout.close_tab(rhythm).unwrap();
        assert_eq!(closed.bindings, vec![PaneBindingEffect::Detach(rhythm)]);
        for effect in closed.bindings {
            effect.apply(&mut binding, &mut session).unwrap();
        }
        assert!(!binding.contains(rhythm.0));
        assert!(layout.document().views.contains_key(&rhythm.0));
        assert_eq!(layout.placement(rhythm), None);

        let reopened = layout.reopen_tab(rhythm).unwrap();
        assert_eq!(reopened.bindings.len(), 1);
        let initial = reopened.bindings[0]
            .apply(&mut binding, &mut session)
            .unwrap();
        assert!(initial.is_some());
        assert!(binding.contains(rhythm.0));
        assert!(layout.placement(rhythm).is_some());
        assert_eq!(PaneInstanceId(rhythm.0), rhythm);
    }

    #[test]
    fn layout_focus_and_presentation_state_round_trip_in_document_extension() {
        let mut layout = layout();
        let components = pane(LegacyBuiltinView::Components);
        layout.focus_pane(components).unwrap();
        layout
            .update_presentation_memory(
                components,
                PanePresentationMemory {
                    scroll: PaneScrollState {
                        horizontal: 17.5,
                        vertical: 201.25,
                    },
                    focus_region: Some("frequency-ruler".into()),
                    reopen_at: None,
                },
            )
            .unwrap();
        let document = layout.export_document().unwrap();
        let json = document.to_json_pretty().unwrap();
        let restored_document = WorkspaceDocument::from_json(&json).unwrap();
        let restored =
            WorkspaceSessionLayout::from_document(ProjectSessionId(9), restored_document).unwrap();
        assert_eq!(
            restored.focused_pane(WorkspaceWindow::Main),
            Some(components)
        );
        assert_eq!(
            restored.presentation_memory(components).unwrap().scroll,
            PaneScrollState {
                horizontal: 17.5,
                vertical: 201.25,
            }
        );
        assert_eq!(
            restored
                .presentation_memory(components)
                .unwrap()
                .focus_region
                .as_deref(),
            Some("frequency-ruler")
        );
    }

    #[test]
    fn dock_snapshots_and_native_window_geometry_round_trip_without_rebinding() {
        let mut layout = layout();
        let placement = WindowPlacement {
            mode: WindowMode::Windowed,
            x: 120.0,
            y: 88.0,
            width: 1440.0,
            height: 900.0,
        };
        let geometry = layout
            .set_window_placement(WorkspaceWindow::Main, Some(placement))
            .unwrap();
        assert!(geometry.bindings.is_empty());
        assert!(geometry.windows.is_empty());

        let dock_snapshot = layout.document().main_layout.clone();
        let dock = layout
            .replace_window_layout(WorkspaceWindow::Main, dock_snapshot)
            .unwrap();
        assert!(dock.bindings.is_empty());
        assert!(dock.windows.is_empty());

        let document = layout.export_document().unwrap();
        let restored = WorkspaceSessionLayout::from_document(
            ProjectSessionId(9),
            WorkspaceDocument::from_json(&document.to_json_pretty().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.document().main_window, Some(placement));
    }

    #[test]
    fn docking_a_window_preserves_binding_and_focuses_same_instances() {
        let mut layout = layout();
        let loom = pane(LegacyBuiltinView::Loom);
        let floated = layout.tear_off_pane(loom, None).unwrap();
        let window = floated
            .windows
            .iter()
            .find_map(|effect| match effect {
                NativeWindowEffect::Open { window, .. } => Some(*window),
                _ => None,
            })
            .unwrap();
        let docked = layout.dock_window_on_close(window).unwrap();
        assert!(docked.bindings.is_empty());
        assert_eq!(
            layout.placement(loom).unwrap().window,
            WorkspaceWindow::Main
        );
        assert_eq!(layout.focused_pane(WorkspaceWindow::Main), Some(loom));
        assert!(docked
            .windows
            .contains(&NativeWindowEffect::Close { window }));
    }

    #[test]
    fn pane_selection_stays_in_one_project_session_across_layout_moves() {
        let mut layout = layout();
        let rhythm = pane(LegacyBuiltinView::Rhythm);
        let mut session = ProjectSession::new(ProjectSessionId(9)).unwrap();
        let mut binding = PaneSessionBinding::new();
        for effect in layout.initial_binding_effects() {
            effect.apply(&mut binding, &mut session).unwrap();
        }
        let selection = ProjectSelection {
            aspect: Some(Aspect::Time(FrameSpan { start: 10, end: 20 })),
            signal: Some(SignalLayer::Source),
            ..ProjectSelection::default()
        };
        binding
            .publish_semantic_selection(&mut session, rhythm.0, selection.clone())
            .unwrap();
        let move_effects = layout.tear_off_pane(rhythm, None).unwrap();
        assert!(move_effects.bindings.is_empty());
        assert_eq!(session.selection().selection, selection);
        assert_eq!(layout.session_id(), session.id());
        assert!(binding.contains(rhythm.0));
    }

    #[test]
    fn macos_overlay_reserves_measured_traffic_lights_and_content_modes() {
        let overlay = resolve_titlebar_layout(TitlebarLayoutInput {
            platform: WindowPlatform::MacOs,
            composition: TitlebarComposition::OverlayTabs,
            titlebar_height: 38.0,
            traffic_lights: Some(LogicalRect {
                x: 12.0,
                y: 10.0,
                width: 56.0,
                height: 14.0,
            }),
            custom_leading_width: 24.0,
            custom_trailing_width: 10.0,
            clearance: 12.0,
        })
        .unwrap();
        assert_eq!(overlay.content.top, 0.0);
        assert_eq!(overlay.pane_group_leading, 80.0);
        assert_eq!(overlay.pane_group_trailing, 22.0);
        assert_eq!(overlay.draggable_height, 38.0);

        let below = resolve_titlebar_layout(TitlebarLayoutInput {
            composition: TitlebarComposition::ContentBelowTitlebar,
            ..TitlebarLayoutInput {
                platform: WindowPlatform::MacOs,
                composition: TitlebarComposition::OverlayTabs,
                titlebar_height: 38.0,
                traffic_lights: None,
                custom_leading_width: 0.0,
                custom_trailing_width: 0.0,
                clearance: 12.0,
            }
        })
        .unwrap();
        assert_eq!(below.content.top, 38.0);
        assert_eq!(below.guise_titlebar_insets(), (0.0, 0.0));
        assert_eq!(below.draggable_height, 0.0);

        assert_eq!(
            resolve_titlebar_layout(TitlebarLayoutInput {
                titlebar_height: f32::NAN,
                ..TitlebarLayoutInput {
                    platform: WindowPlatform::MacOs,
                    composition: TitlebarComposition::OverlayTabs,
                    titlebar_height: 38.0,
                    traffic_lights: None,
                    custom_leading_width: 0.0,
                    custom_trailing_width: 0.0,
                    clearance: 12.0,
                }
            }),
            Err(WorkspaceSessionLayoutError::InvalidTitlebarMetrics)
        );
    }

    #[test]
    fn shared_titlebar_policy_keeps_tabs_and_custom_chrome_consistent() {
        let overlay = default_workspace_titlebar_layout(
            WindowPlatform::MacOs,
            TitlebarComposition::OverlayTabs,
            None,
        )
        .unwrap();
        assert_eq!(overlay.pane_group_leading, 82.0);
        assert_eq!(overlay.content.top, 0.0);

        let below = default_workspace_titlebar_layout(
            WindowPlatform::MacOs,
            TitlebarComposition::ContentBelowTitlebar,
            None,
        )
        .unwrap();
        assert_eq!(below.content.top, WORKSPACE_TITLEBAR_HEIGHT);
        assert_eq!(below.pane_group_leading, 0.0);

        let other = default_workspace_titlebar_layout(
            WindowPlatform::Other,
            TitlebarComposition::OverlayTabs,
            None,
        )
        .unwrap();
        assert_eq!(other.guise_titlebar_insets(), (0.0, 0.0));
    }
}
