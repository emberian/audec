//! Toolkit-neutral workspace semantics and keyboard action resolution.
//!
//! GPUI 0.2.2 has focus handles and typed actions, but no API for emitting a
//! native accessibility element, role, name, selected state, or action set.
//! This tree is therefore the honest adapter boundary: hosts can drive it from
//! keyboard/menu actions today and feed the same stable nodes to a future GPUI
//! accessibility bridge without reconstructing workspace meaning from pixels.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::workspace::native_authority::WorkspaceLayoutCommand;
use crate::workspace_document::{
    CloseBehavior, DockLayout, DockPaneId, WorkspaceItemKind, WorkspaceViewId,
};
use crate::workspace_session_layout::{
    PaneInstanceId, PaneMoveDestination, WorkspaceSessionLayout, WorkspaceWindow,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceSemanticNodeId {
    Workspace,
    Window(WorkspaceWindow),
    SplitGroup {
        window: WorkspaceWindow,
        ordinal: u32,
    },
    Pane(DockPaneId),
    TabList(DockPaneId),
    Tab(WorkspaceViewId),
    Content(WorkspaceViewId),
    CloseControl(WorkspaceViewId),
    FloatDockControl(WorkspaceViewId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSemanticRole {
    Workspace,
    Window,
    SplitGroup,
    Pane,
    TabList,
    Tab,
    Region,
    Button,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceSemanticState {
    pub selected: bool,
    pub focused: bool,
    pub hidden: bool,
    pub disabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSemanticAction {
    Focus,
    Activate,
    Close,
    FloatOrDock,
    NextTab,
    PreviousTab,
    NextPane,
    PreviousPane,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSemanticNode {
    pub id: WorkspaceSemanticNodeId,
    pub role: WorkspaceSemanticRole,
    pub label: String,
    pub state: WorkspaceSemanticState,
    pub actions: Vec<WorkspaceSemanticAction>,
    pub children: Vec<WorkspaceSemanticNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSemanticTree {
    pub revision: u64,
    pub root: WorkspaceSemanticNode,
}

impl WorkspaceSemanticTree {
    pub fn from_layout(layout: &WorkspaceSessionLayout) -> Self {
        let mut windows = Vec::with_capacity(layout.document().floating_windows.len() + 1);
        windows.push(window_node(
            layout,
            WorkspaceWindow::Main,
            "Main workspace".into(),
            &layout.document().main_layout,
        ));
        for (&id, floating) in &layout.document().floating_windows {
            windows.push(window_node(
                layout,
                WorkspaceWindow::Floating(id),
                format!("Floating workspace {}", id.0),
                &floating.layout,
            ));
        }
        Self {
            revision: layout.revision(),
            root: WorkspaceSemanticNode {
                id: WorkspaceSemanticNodeId::Workspace,
                role: WorkspaceSemanticRole::Workspace,
                label: "Project workspace".into(),
                state: WorkspaceSemanticState::default(),
                actions: vec![
                    WorkspaceSemanticAction::NextPane,
                    WorkspaceSemanticAction::PreviousPane,
                ],
                children: windows,
            },
        }
    }

    pub fn find(&self, id: WorkspaceSemanticNodeId) -> Option<&WorkspaceSemanticNode> {
        fn find_in(
            node: &WorkspaceSemanticNode,
            id: WorkspaceSemanticNodeId,
        ) -> Option<&WorkspaceSemanticNode> {
            if node.id == id {
                return Some(node);
            }
            node.children.iter().find_map(|child| find_in(child, id))
        }
        find_in(&self.root, id)
    }
}

fn window_node(
    session: &WorkspaceSessionLayout,
    window: WorkspaceWindow,
    label: String,
    layout: &DockLayout,
) -> WorkspaceSemanticNode {
    let mut split_ordinal = 0;
    let content = layout_node(session, window, layout, &mut split_ordinal);
    WorkspaceSemanticNode {
        id: WorkspaceSemanticNodeId::Window(window),
        role: WorkspaceSemanticRole::Window,
        label,
        state: WorkspaceSemanticState {
            focused: session.focused_pane(window).is_some(),
            ..Default::default()
        },
        actions: vec![
            WorkspaceSemanticAction::NextPane,
            WorkspaceSemanticAction::PreviousPane,
        ],
        children: vec![content],
    }
}

fn layout_node(
    session: &WorkspaceSessionLayout,
    window: WorkspaceWindow,
    layout: &DockLayout,
    split_ordinal: &mut u32,
) -> WorkspaceSemanticNode {
    match layout {
        DockLayout::Pane {
            pane_id,
            items,
            active,
            ..
        } => {
            let focused = session.focused_pane(window);
            let tabs = items
                .iter()
                .enumerate()
                .map(|(index, &view)| {
                    let descriptor = &session.document().views[&view];
                    let selected = index == *active;
                    let is_focused = focused == Some(PaneInstanceId(view));
                    let close_disabled = descriptor.kind.close_behavior() == CloseBehavior::Pinned;
                    let float_disabled = !descriptor.kind.can_float();
                    let location_label = match window {
                        WorkspaceWindow::Main => "Float",
                        WorkspaceWindow::Floating(_) => "Dock",
                    };
                    WorkspaceSemanticNode {
                        id: WorkspaceSemanticNodeId::Tab(view),
                        role: WorkspaceSemanticRole::Tab,
                        label: descriptor
                            .title_override
                            .clone()
                            .unwrap_or_else(|| kind_label(&descriptor.kind).into()),
                        state: WorkspaceSemanticState {
                            selected,
                            focused: is_focused,
                            ..Default::default()
                        },
                        actions: vec![
                            WorkspaceSemanticAction::Focus,
                            WorkspaceSemanticAction::Activate,
                            WorkspaceSemanticAction::Close,
                            WorkspaceSemanticAction::FloatOrDock,
                        ],
                        children: vec![
                            WorkspaceSemanticNode {
                                id: WorkspaceSemanticNodeId::Content(view),
                                role: WorkspaceSemanticRole::Region,
                                label: format!("{} editor", kind_label(&descriptor.kind)),
                                state: WorkspaceSemanticState {
                                    selected,
                                    focused: is_focused,
                                    hidden: !selected,
                                    ..Default::default()
                                },
                                actions: vec![WorkspaceSemanticAction::Focus],
                                children: Vec::new(),
                            },
                            WorkspaceSemanticNode {
                                id: WorkspaceSemanticNodeId::CloseControl(view),
                                role: WorkspaceSemanticRole::Button,
                                label: "Close tab".into(),
                                state: WorkspaceSemanticState {
                                    disabled: close_disabled,
                                    ..Default::default()
                                },
                                actions: vec![WorkspaceSemanticAction::Close],
                                children: Vec::new(),
                            },
                            WorkspaceSemanticNode {
                                id: WorkspaceSemanticNodeId::FloatDockControl(view),
                                role: WorkspaceSemanticRole::Button,
                                label: format!("{location_label} tab"),
                                state: WorkspaceSemanticState {
                                    disabled: float_disabled,
                                    ..Default::default()
                                },
                                actions: vec![WorkspaceSemanticAction::FloatOrDock],
                                children: Vec::new(),
                            },
                        ],
                    }
                })
                .collect();
            WorkspaceSemanticNode {
                id: WorkspaceSemanticNodeId::Pane(*pane_id),
                role: WorkspaceSemanticRole::Pane,
                label: format!("Editor pane {}", pane_id.0),
                state: WorkspaceSemanticState {
                    focused: focused.is_some_and(|pane| items.contains(&pane.0)),
                    ..Default::default()
                },
                actions: vec![
                    WorkspaceSemanticAction::NextTab,
                    WorkspaceSemanticAction::PreviousTab,
                    WorkspaceSemanticAction::NextPane,
                    WorkspaceSemanticAction::PreviousPane,
                ],
                children: vec![WorkspaceSemanticNode {
                    id: WorkspaceSemanticNodeId::TabList(*pane_id),
                    role: WorkspaceSemanticRole::TabList,
                    label: format!("Tabs in pane {}", pane_id.0),
                    state: WorkspaceSemanticState::default(),
                    actions: vec![
                        WorkspaceSemanticAction::NextTab,
                        WorkspaceSemanticAction::PreviousTab,
                    ],
                    children: tabs,
                }],
            }
        }
        DockLayout::Split {
            axis,
            first,
            second,
            ..
        } => {
            let ordinal = *split_ordinal;
            *split_ordinal += 1;
            WorkspaceSemanticNode {
                id: WorkspaceSemanticNodeId::SplitGroup { window, ordinal },
                role: WorkspaceSemanticRole::SplitGroup,
                label: format!("{axis:?} split"),
                state: WorkspaceSemanticState::default(),
                actions: vec![
                    WorkspaceSemanticAction::NextPane,
                    WorkspaceSemanticAction::PreviousPane,
                ],
                children: vec![
                    layout_node(session, window, first, split_ordinal),
                    layout_node(session, window, second, split_ordinal),
                ],
            }
        }
    }
}

fn kind_label(kind: &WorkspaceItemKind) -> &'static str {
    match kind {
        WorkspaceItemKind::Overview => "Overview",
        WorkspaceItemKind::Arrangement => "Arrangement",
        WorkspaceItemKind::Browser => "Browser",
        WorkspaceItemKind::Inspector => "Inspector",
        WorkspaceItemKind::PatternEditor { .. } => "Pattern editor",
        WorkspaceItemKind::AutomationEditor => "Automation editor",
        WorkspaceItemKind::Mixer => "Mixer",
        WorkspaceItemKind::AnalysisLens { .. } => "Analysis",
        WorkspaceItemKind::Render => "Render",
        WorkspaceItemKind::Extension { .. } => "Extension",
    }
}

/// Resolve a semantic or keyboard action into the same typed command accepted
/// by [`WorkspaceCommandAuthority`](super::native_authority::WorkspaceCommandAuthority).
pub fn command_for_semantic_action(
    layout: &WorkspaceSessionLayout,
    node: WorkspaceSemanticNodeId,
    action: WorkspaceSemanticAction,
) -> Result<WorkspaceLayoutCommand, WorkspaceSemanticError> {
    let pane = view_for_node(node)
        .or_else(|| pane_view_for_navigation(layout, node, action))
        .ok_or(WorkspaceSemanticError::NodeHasNoPane(node))?;
    match action {
        WorkspaceSemanticAction::Focus | WorkspaceSemanticAction::Activate => {
            Ok(WorkspaceLayoutCommand::FocusPane(PaneInstanceId(pane)))
        }
        WorkspaceSemanticAction::Close => {
            Ok(WorkspaceLayoutCommand::CloseTab(PaneInstanceId(pane)))
        }
        WorkspaceSemanticAction::FloatOrDock => {
            let placement = layout
                .placement(PaneInstanceId(pane))
                .ok_or(WorkspaceSemanticError::PaneHidden(pane))?;
            match placement.window {
                WorkspaceWindow::Main => Ok(WorkspaceLayoutCommand::TearOffPane {
                    pane: PaneInstanceId(pane),
                    placement: None,
                }),
                WorkspaceWindow::Floating(_) => Ok(WorkspaceLayoutCommand::MovePane {
                    pane: PaneInstanceId(pane),
                    destination: PaneMoveDestination {
                        window: WorkspaceWindow::Main,
                        dock_pane: layout.document().main_layout.primary_pane(),
                        tab_index: usize::MAX,
                    },
                }),
            }
        }
        WorkspaceSemanticAction::NextTab
        | WorkspaceSemanticAction::PreviousTab
        | WorkspaceSemanticAction::NextPane
        | WorkspaceSemanticAction::PreviousPane => {
            Ok(WorkspaceLayoutCommand::FocusPane(PaneInstanceId(pane)))
        }
    }
}

fn view_for_node(node: WorkspaceSemanticNodeId) -> Option<WorkspaceViewId> {
    match node {
        WorkspaceSemanticNodeId::Tab(view)
        | WorkspaceSemanticNodeId::Content(view)
        | WorkspaceSemanticNodeId::CloseControl(view)
        | WorkspaceSemanticNodeId::FloatDockControl(view) => Some(view),
        _ => None,
    }
}

fn pane_view_for_navigation(
    layout: &WorkspaceSessionLayout,
    node: WorkspaceSemanticNodeId,
    action: WorkspaceSemanticAction,
) -> Option<WorkspaceViewId> {
    let mut panes = BTreeMap::new();
    collect_panes(&layout.document().main_layout, &mut panes);
    for floating in layout.document().floating_windows.values() {
        collect_panes(&floating.layout, &mut panes);
    }
    let pane_id = match node {
        WorkspaceSemanticNodeId::Pane(pane) | WorkspaceSemanticNodeId::TabList(pane) => pane,
        _ => layout
            .focused_pane(WorkspaceWindow::Main)
            .and_then(|pane| layout.placement(pane))
            .map(|placement| placement.dock_pane)?,
    };
    match action {
        WorkspaceSemanticAction::NextTab | WorkspaceSemanticAction::PreviousTab => {
            let (items, active) = panes.get(&pane_id)?;
            let next = if action == WorkspaceSemanticAction::NextTab {
                (active + 1) % items.len()
            } else {
                (active + items.len() - 1) % items.len()
            };
            items.get(next).copied()
        }
        WorkspaceSemanticAction::NextPane | WorkspaceSemanticAction::PreviousPane => {
            let ids = panes.keys().copied().collect::<Vec<_>>();
            let index = ids.iter().position(|candidate| *candidate == pane_id)?;
            let next = if action == WorkspaceSemanticAction::NextPane {
                (index + 1) % ids.len()
            } else {
                (index + ids.len() - 1) % ids.len()
            };
            let (items, active) = panes.get(&ids[next])?;
            items.get(*active).copied()
        }
        _ => None,
    }
}

fn collect_panes(
    layout: &DockLayout,
    panes: &mut BTreeMap<DockPaneId, (Vec<WorkspaceViewId>, usize)>,
) {
    match layout {
        DockLayout::Pane {
            pane_id,
            items,
            active,
            ..
        } => {
            panes.insert(*pane_id, (items.clone(), *active));
        }
        DockLayout::Split { first, second, .. } => {
            collect_panes(first, panes);
            collect_panes(second, panes);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSemanticError {
    NodeHasNoPane(WorkspaceSemanticNodeId),
    PaneHidden(WorkspaceViewId),
}

impl fmt::Display for WorkspaceSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeHasNoPane(node) => write!(
                formatter,
                "semantic workspace node {node:?} has no pane action"
            ),
            Self::PaneHidden(view) => write!(formatter, "workspace view {} is hidden", view.0),
        }
    }
}

impl Error for WorkspaceSemanticError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_session::ProjectSessionId;
    use crate::workspace_document::{LegacyBuiltinView, WorkspaceDocument};

    fn layout() -> WorkspaceSessionLayout {
        WorkspaceSessionLayout::from_document(ProjectSessionId(51), WorkspaceDocument::default())
            .unwrap()
    }

    #[test]
    fn tree_exposes_selected_focused_tabs_and_controls() {
        let layout = layout();
        let tree = WorkspaceSemanticTree::from_layout(&layout);
        let view = layout.focused_pane(WorkspaceWindow::Main).unwrap().0;
        let tab = tree.find(WorkspaceSemanticNodeId::Tab(view)).unwrap();
        assert_eq!(tab.role, WorkspaceSemanticRole::Tab);
        assert!(tab.state.selected);
        assert!(tab.state.focused);
        assert!(tree
            .find(WorkspaceSemanticNodeId::CloseControl(view))
            .is_some());
    }

    #[test]
    fn next_tab_wraps_and_resolves_to_typed_focus_command() {
        let layout = layout();
        let pane = layout.document().main_layout.primary_pane();
        let expected = match &layout.document().main_layout {
            DockLayout::Pane { items, .. } => *items.last().unwrap(),
            DockLayout::Split { first, .. } => match first.as_ref() {
                DockLayout::Pane { items, .. } => *items.last().unwrap(),
                DockLayout::Split { .. } => panic!("unexpected nested default layout"),
            },
        };
        let command = command_for_semantic_action(
            &layout,
            WorkspaceSemanticNodeId::TabList(pane),
            WorkspaceSemanticAction::PreviousTab,
        )
        .unwrap();
        let WorkspaceLayoutCommand::FocusPane(target) = command else {
            panic!("expected focus command")
        };
        assert_eq!(target.0, expected);
    }

    #[test]
    fn float_and_dock_share_one_semantic_action() {
        let mut layout = layout();
        let pane = PaneInstanceId(LegacyBuiltinView::Waterfall.id());
        assert!(matches!(
            command_for_semantic_action(
                &layout,
                WorkspaceSemanticNodeId::FloatDockControl(pane.0),
                WorkspaceSemanticAction::FloatOrDock,
            )
            .unwrap(),
            WorkspaceLayoutCommand::TearOffPane { pane: actual, .. } if actual == pane
        ));
        layout.tear_off_pane(pane, None).unwrap();
        assert!(matches!(
            command_for_semantic_action(
                &layout,
                WorkspaceSemanticNodeId::FloatDockControl(pane.0),
                WorkspaceSemanticAction::FloatOrDock,
            )
            .unwrap(),
            WorkspaceLayoutCommand::MovePane { pane: actual, destination }
                if actual == pane && destination.window == WorkspaceWindow::Main
        ));
    }

    #[test]
    fn portable_json_roundtrip_preserves_semantic_node_ids() {
        let layout = layout();
        let before = WorkspaceSemanticTree::from_layout(&layout);
        let json = serde_json::to_string(&layout.export_document().unwrap()).unwrap();
        let document = serde_json::from_str(&json).unwrap();
        let restored =
            WorkspaceSessionLayout::from_document(ProjectSessionId(51), document).unwrap();
        let after = WorkspaceSemanticTree::from_layout(&restored);
        assert_eq!(before.root, after.root);
    }
}
