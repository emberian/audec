//! Product-semantic workspace surfaces and keyboard action resolution.
//!
//! This tree is Audec's meaning and identity authority; the current owned GPUI
//! fork can now lower it into real AccessKit nodes and actions through
//! [`platform_semantics`]. Keeping the product tree separate still matters:
//! canvas virtualization, durable object identity, disabled reasons and
//! command epochs must not be reconstructed from paint traversal. The native
//! bridge is real, but platform screen-reader walkthroughs remain a separate
//! release gate.

#[path = "platform_semantics.rs"]
pub mod platform_semantics;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::ui_actions::{
    ActionContext, ActionId, ActionRegistry, ActionState, InvocationModifiers,
};
use crate::workspace::native_authority::WorkspaceLayoutCommand;
use crate::workspace_document::{
    CloseBehavior, DockLayout, DockPaneId, ViewLocation, WorkspaceItemKind, WorkspaceViewId,
};
use crate::workspace_items::EditorTarget;
use crate::workspace_session_layout::{
    PaneInstanceId, PaneMoveDestination, WorkspaceSessionLayout, WorkspaceWindow,
};

/// Version of the toolkit-neutral projection consumed by native UI and
/// accessibility adapters. This is an in-process contract, not a project-file
/// codec, but naming the version prevents an adapter from silently guessing
/// when the semantic vocabulary changes.
pub const SEMANTIC_PROJECTION_SCHEMA_VERSION: u32 = 1;

/// Stable identity of a node within one persisted workspace view.
///
/// The producer owns `local_key`: it must derive from semantic object identity
/// (for example `clip/41`), never list position, paint order, or a translated
/// label. Keeping the readable key is intentional: accessibility inspection
/// and UI automation should be able to explain which product object they saw.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemanticSurfaceNodeId {
    pub view: WorkspaceViewId,
    pub local_key: String,
}

impl SemanticSurfaceNodeId {
    pub fn new(view: WorkspaceViewId, local_key: impl Into<String>) -> Self {
        Self {
            view,
            local_key: local_key.into(),
        }
    }

    pub fn root(view: WorkspaceViewId) -> Self {
        Self::new(view, "surface")
    }

    pub fn object(
        view: WorkspaceViewId,
        object_kind: &'static str,
        object_id: impl fmt::Display,
    ) -> Self {
        Self::new(view, format!("{object_kind}/{object_id}"))
    }

    pub fn part(
        view: WorkspaceViewId,
        object_kind: &'static str,
        object_id: impl fmt::Display,
        part: &'static str,
    ) -> Self {
        Self::new(view, format!("{object_kind}/{object_id}/{part}"))
    }
}

/// Product semantics, deliberately richer than any one native toolkit's role
/// enum. A GPUI or AccessKit adapter may lower several Audec-specific roles to
/// the same native role while retaining this role in inspection snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SemanticSurfaceRole {
    Application,
    Window,
    Workspace,
    Region,
    Group,
    Toolbar,
    TabList,
    Tab,
    TabPanel,
    Button,
    ToggleButton,
    Checkbox,
    TextInput,
    Slider,
    Meter,
    List,
    ListItem,
    Grid,
    Row,
    Cell,
    Tree,
    TreeItem,
    Menu,
    MenuItem,
    Status,
    Alert,
    Graphic,
    Timeline,
    Waveform,
    Spectrogram,
    Clip,
    Note,
    Step,
    AutomationPoint,
    MixerChannel,
}

/// Whether a projected node is on screen. Offscreen retained nodes are
/// permitted only by an explicit canvas policy and must not be translated to
/// a native "visible" claim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SemanticVisibility {
    #[default]
    Visible,
    OffscreenRetained,
    Hidden,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SemanticSurfaceState {
    pub visibility: SemanticVisibility,
    pub focused: bool,
    pub disabled: bool,
    pub disabled_reason: Option<String>,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub busy: bool,
}

/// Selection and ordered-collection metadata are independent from focus.
/// Positions are one-based when present, matching native accessibility APIs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SemanticSelectionState {
    pub selected: bool,
    pub primary: bool,
    pub position_in_set: Option<u64>,
    pub set_size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticNumericValue {
    pub current: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub step: Option<f64>,
    pub formatted: String,
    pub unit: Option<String>,
}

/// Values remain product data. Adapters may expose the numeric fields to
/// native increment/decrement APIs and the formatted text to screen readers.
#[derive(Clone, Debug, PartialEq)]
pub enum SemanticSurfaceValue {
    Text(String),
    Numeric(SemanticNumericValue),
    /// Exact project-frame position plus a musician-facing rendering.
    ProjectFrame {
        frame: i64,
        formatted: String,
    },
}

/// One action exposed by a semantic node. The action registry remains the
/// sole label, keybinding, scope and enabled-state authority; semantic nodes
/// retain the registry's resolution for the exact projected context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticActionBinding {
    pub id: ActionId,
    pub state: ActionState,
    /// Native invocation semantics are explicit. A registry action is not
    /// assumed to be a click, increment, or value edit merely because it is
    /// attached to a particular visual widget.
    pub native: SemanticNativeAction,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SemanticNativeAction {
    /// A named AccessKit custom action. This is the conservative default for
    /// commands whose producer has not declared a standard interaction.
    #[default]
    Custom,
    Activate,
    Focus,
    Increment,
    Decrement,
    Expand,
    Collapse,
    SetValue,
    ShowContextMenu,
}

impl SemanticActionBinding {
    pub fn resolve(
        registry: &ActionRegistry,
        id: ActionId,
        context: &ActionContext,
    ) -> Option<Self> {
        registry.resolve(id, context).map(|state| Self {
            id,
            state,
            native: SemanticNativeAction::Custom,
        })
    }

    pub fn resolve_as(
        registry: &ActionRegistry,
        id: ActionId,
        context: &ActionContext,
        native: SemanticNativeAction,
    ) -> Option<Self> {
        Self::resolve(registry, id, context).map(|binding| Self { native, ..binding })
    }
}

/// One stable product node. Children are ordered semantically, not by paint
/// traversal. Canvas producers should use [`scope_canvas_semantic_children`]
/// rather than placing their entire project into this vector.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticSurfaceNode {
    pub id: SemanticSurfaceNodeId,
    pub role: SemanticSurfaceRole,
    pub label: String,
    pub description: Option<String>,
    pub value: Option<SemanticSurfaceValue>,
    pub state: SemanticSurfaceState,
    pub selection: SemanticSelectionState,
    pub target: Option<EditorTarget>,
    pub actions: Vec<SemanticActionBinding>,
    /// Summary of logical canvas children which were intentionally not
    /// materialized into the semantic tree for this viewport.
    pub canvas_summary: Option<CanvasSemanticSummary>,
    pub children: Vec<SemanticSurfaceNode>,
}

impl SemanticSurfaceNode {
    pub fn new(
        id: SemanticSurfaceNodeId,
        role: SemanticSurfaceRole,
        label: impl Into<String>,
    ) -> Self {
        Self {
            id,
            role,
            label: label.into(),
            description: None,
            value: None,
            state: SemanticSurfaceState::default(),
            selection: SemanticSelectionState::default(),
            target: None,
            actions: Vec::new(),
            canvas_summary: None,
            children: Vec::new(),
        }
    }

    pub fn find(&self, id: &SemanticSurfaceNodeId) -> Option<&Self> {
        if &self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(id))
    }
}

/// Monotonic identity of the product state from which an adapter projection
/// was made. Every action from native UI or assistive technology returns this
/// stamp so an action can never silently hit a replacement object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemanticProjectionStamp {
    pub view: WorkspaceViewId,
    pub revision: u64,
}

/// Toolkit-neutral authoritative semantic surface.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticSurface {
    pub stamp: SemanticProjectionStamp,
    pub root: SemanticSurfaceNode,
}

impl SemanticSurface {
    pub fn new(
        view: WorkspaceViewId,
        revision: u64,
        root: SemanticSurfaceNode,
    ) -> Result<Self, SemanticSurfaceError> {
        let surface = Self {
            stamp: SemanticProjectionStamp { view, revision },
            root,
        };
        surface.validate()?;
        Ok(surface)
    }

    pub fn validate(&self) -> Result<(), SemanticSurfaceError> {
        let mut ids = BTreeSet::new();
        validate_surface_node(&self.root, self.stamp.view, &mut ids)
    }

    pub fn project(&self) -> SemanticProjection {
        let mut nodes = Vec::new();
        flatten_semantic_node(&self.root, None, 0, &mut nodes);
        SemanticProjection {
            schema_version: SEMANTIC_PROJECTION_SCHEMA_VERSION,
            stamp: self.stamp,
            root: self.root.id.clone(),
            nodes,
        }
    }

    /// Resolve any keyboard, menu, palette, pointer or assistive-tech request
    /// through the exact same [`ActionId`] advertised by the registry.
    pub fn route_action(
        &self,
        request: &SemanticActionRequest,
    ) -> Result<SemanticActionDispatch, SemanticSurfaceError> {
        if request.projection != self.stamp {
            return Err(SemanticSurfaceError::StaleProjection {
                expected: self.stamp,
                received: request.projection,
            });
        }
        let node = self
            .root
            .find(&request.node)
            .ok_or_else(|| SemanticSurfaceError::UnknownSurfaceNode(request.node.clone()))?;
        let binding = node
            .actions
            .iter()
            .find(|binding| binding.id == request.action)
            .ok_or_else(|| SemanticSurfaceError::ActionNotExposed {
                node: request.node.clone(),
                action: request.action,
            })?;
        if !binding.state.enabled {
            return Err(SemanticSurfaceError::SurfaceActionDisabled {
                node: request.node.clone(),
                action: request.action,
                reason: binding.state.disabled_reason,
            });
        }
        Ok(SemanticActionDispatch {
            action: request.action,
            origin: request.origin,
            view: Some(request.node.view),
            target: node.target.clone(),
            modifiers: request.modifiers,
        })
    }
}

fn validate_surface_node(
    node: &SemanticSurfaceNode,
    view: WorkspaceViewId,
    ids: &mut BTreeSet<SemanticSurfaceNodeId>,
) -> Result<(), SemanticSurfaceError> {
    if node.id.view != view {
        return Err(SemanticSurfaceError::ForeignSurfaceNode {
            expected: view,
            node: node.id.clone(),
        });
    }
    if node.id.local_key.trim().is_empty() {
        return Err(SemanticSurfaceError::EmptySurfaceNodeKey(node.id.clone()));
    }
    if !ids.insert(node.id.clone()) {
        return Err(SemanticSurfaceError::DuplicateSurfaceNode(node.id.clone()));
    }
    let mut action_ids = BTreeSet::new();
    for binding in &node.actions {
        if !action_ids.insert(binding.id) {
            return Err(SemanticSurfaceError::DuplicateSurfaceAction {
                node: node.id.clone(),
                action: binding.id,
            });
        }
    }
    for child in &node.children {
        validate_surface_node(child, view, ids)?;
    }
    Ok(())
}

/// A flattened node ready for either a retained GPUI registration pass or an
/// AccessKit tree update. Geometry and native focus handles intentionally stay
/// with the adapter that owns the current window.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedSemanticNode {
    pub id: SemanticSurfaceNodeId,
    pub parent: Option<SemanticSurfaceNodeId>,
    pub child_index: usize,
    pub role: SemanticSurfaceRole,
    pub label: String,
    pub description: Option<String>,
    pub value: Option<SemanticSurfaceValue>,
    pub state: SemanticSurfaceState,
    pub selection: SemanticSelectionState,
    pub target: Option<EditorTarget>,
    pub actions: Vec<SemanticActionBinding>,
    pub canvas_summary: Option<CanvasSemanticSummary>,
}

fn flatten_semantic_node(
    node: &SemanticSurfaceNode,
    parent: Option<&SemanticSurfaceNodeId>,
    child_index: usize,
    output: &mut Vec<ProjectedSemanticNode>,
) {
    output.push(ProjectedSemanticNode {
        id: node.id.clone(),
        parent: parent.cloned(),
        child_index,
        role: node.role,
        label: node.label.clone(),
        description: node.description.clone(),
        value: node.value.clone(),
        state: node.state.clone(),
        selection: node.selection,
        target: node.target.clone(),
        actions: node.actions.clone(),
        canvas_summary: node.canvas_summary.clone(),
    });
    for (index, child) in node.children.iter().enumerate() {
        flatten_semantic_node(child, Some(&node.id), index, output);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticProjection {
    pub schema_version: u32,
    pub stamp: SemanticProjectionStamp,
    pub root: SemanticSurfaceNodeId,
    /// Deterministic semantic pre-order. Adapters must replace a surface
    /// atomically and remove nodes absent from the replacement projection.
    pub nodes: Vec<ProjectedSemanticNode>,
}

/// Orders projection installation independently of the native platform. An
/// analysis result or delayed render from revision N cannot replace the
/// already-installed semantics for revision N+1.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SemanticProjectionCoordinator {
    installed: BTreeMap<WorkspaceViewId, SemanticProjectionStamp>,
}

impl SemanticProjectionCoordinator {
    pub fn installed(&self, view: WorkspaceViewId) -> Option<SemanticProjectionStamp> {
        self.installed.get(&view).copied()
    }

    /// Admit `projection` as the installed semantics for its view. The caller
    /// performs the native install after this returns `Ok`, so a delayed
    /// projection is refused before it can touch the platform tree.
    pub fn replace(&mut self, projection: &SemanticProjection) -> Result<(), SemanticSurfaceError> {
        if projection.schema_version != SEMANTIC_PROJECTION_SCHEMA_VERSION {
            return Err(SemanticSurfaceError::UnsupportedProjectionSchema {
                expected: SEMANTIC_PROJECTION_SCHEMA_VERSION,
                received: projection.schema_version,
            });
        }
        if let Some(current) = self.installed(projection.stamp.view) {
            if projection.stamp.revision < current.revision {
                return Err(SemanticSurfaceError::StaleProjection {
                    expected: current,
                    received: projection.stamp,
                });
            }
        }
        self.installed
            .insert(projection.stamp.view, projection.stamp);
        Ok(())
    }

    pub fn remove(
        &mut self,
        view: WorkspaceViewId,
        expected: SemanticProjectionStamp,
    ) -> Result<(), SemanticSurfaceError> {
        if let Some(current) = self.installed(view) {
            if current != expected {
                return Err(SemanticSurfaceError::StaleProjection {
                    expected: current,
                    received: expected,
                });
            }
        }
        self.installed.remove(&view);
        Ok(())
    }
}

/// The origin is retained for diagnostics and policy, but never changes the
/// action identity or handler selected by the application command router.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticActionOrigin {
    Keyboard,
    Menu,
    ContextMenu,
    Palette,
    Pointer,
    AssistiveTechnology,
    ExternalProtocol,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticActionRequest {
    pub projection: SemanticProjectionStamp,
    pub node: SemanticSurfaceNodeId,
    pub action: ActionId,
    pub origin: SemanticActionOrigin,
    pub modifiers: InvocationModifiers,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticActionDispatch {
    pub action: ActionId,
    pub origin: SemanticActionOrigin,
    pub view: Option<WorkspaceViewId>,
    pub target: Option<EditorTarget>,
    pub modifiers: InvocationModifiers,
}

/// A logical visible window over a potentially enormous canvas collection.
/// `first` is zero-based; `count` may extend beyond `total` and is clamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanvasVisibleWindow {
    pub first: u64,
    pub count: u64,
    pub total: u64,
}

impl CanvasVisibleWindow {
    pub fn visible_end(self) -> u64 {
        self.first.saturating_add(self.count).min(self.total)
    }

    pub fn contains(self, ordinal: u64) -> bool {
        ordinal >= self.first && ordinal < self.visible_end()
    }
}

/// Explicit exception to the visible-only canvas rule. Retention keeps focus
/// or selection addressable during a viewport transition, but the resulting
/// node is marked [`SemanticVisibility::OffscreenRetained`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CanvasOffscreenPolicy {
    #[default]
    Omit,
    RetainFocused,
    RetainFocusedAndSelected,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanvasSemanticPolicy {
    pub window: CanvasVisibleWindow,
    pub offscreen: CanvasOffscreenPolicy,
}

impl Default for CanvasVisibleWindow {
    fn default() -> Self {
        Self {
            first: 0,
            count: 0,
            total: 0,
        }
    }
}

/// One object offered by a canvas producer. Ordinals are stable ordering for
/// this collection snapshot, while the node ID remains stable across sorting,
/// viewport changes and rerenders.
#[derive(Clone, Debug, PartialEq)]
pub struct CanvasSemanticChild {
    pub ordinal: u64,
    pub node: SemanticSurfaceNode,
}

/// Honest summary for a virtualized musical canvas. `total` describes the
/// logical collection, while `materialized_*` describe the bounded semantic
/// payload produced for this frame. Assistive technology gets this summary on
/// the canvas node instead of an enormous, misleading hidden subtree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanvasSemanticSummary {
    pub total: u64,
    pub requested_first: u64,
    pub requested_end: u64,
    pub materialized_visible: u64,
    pub retained_offscreen: u64,
    pub omitted: u64,
    pub retained_selected: u64,
    pub retained_focused: u64,
}

impl CanvasSemanticSummary {
    pub fn announcement(&self) -> String {
        let visible = if self.materialized_visible == 0 {
            "no visible items materialized".to_string()
        } else {
            format!(
                "{} visible items materialized from requested positions {} through {}",
                self.materialized_visible,
                self.requested_first.saturating_add(1),
                self.requested_end
            )
        };
        let mut announcement = format!(
            "{} items total; {visible}; {} offscreen items omitted",
            self.total, self.omitted
        );
        if self.retained_offscreen > 0 {
            announcement.push_str(&format!(
                "; {} offscreen items retained outside native traversal",
                self.retained_offscreen
            ));
            if self.retained_selected > 0 || self.retained_focused > 0 {
                announcement.push_str(&format!(
                    " ({} selected, {} focused)",
                    self.retained_selected, self.retained_focused
                ));
            }
        }
        announcement
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScopedCanvasSemantics {
    pub children: Vec<SemanticSurfaceNode>,
    pub summary: CanvasSemanticSummary,
}

impl SemanticSurfaceNode {
    /// Install a bounded canvas projection and its traversal summary together,
    /// so panes cannot accidentally publish one without the other.
    pub fn install_canvas_semantics(&mut self, scoped: ScopedCanvasSemantics) {
        self.children = scoped.children;
        self.canvas_summary = Some(scoped.summary);
    }
}

/// Scope an ordered canvas collection to what the user can currently see.
/// Offscreen children are omitted, not emitted as hidden nodes. This prevents
/// a 100k-note piano roll from becoming a 100k-node accessibility tree.
pub fn scope_canvas_semantic_children(
    children: impl IntoIterator<Item = CanvasSemanticChild>,
    policy: CanvasSemanticPolicy,
) -> Result<Vec<SemanticSurfaceNode>, SemanticSurfaceError> {
    Ok(scope_canvas_semantics(children, policy)?.children)
}

/// Scoped form that also returns the native-traversal summary. New canvas
/// adapters should prefer this and install the result atomically with
/// [`SemanticSurfaceNode::install_canvas_semantics`].
pub fn scope_canvas_semantics(
    children: impl IntoIterator<Item = CanvasSemanticChild>,
    policy: CanvasSemanticPolicy,
) -> Result<ScopedCanvasSemantics, SemanticSurfaceError> {
    if policy.window.first > policy.window.total {
        return Err(SemanticSurfaceError::InvalidVisibleWindow(policy.window));
    }
    let mut ordered = BTreeMap::new();
    for child in children {
        if child.ordinal >= policy.window.total {
            return Err(SemanticSurfaceError::CanvasOrdinalOutOfRange {
                ordinal: child.ordinal,
                total: policy.window.total,
            });
        }
        if ordered.insert(child.ordinal, child.node).is_some() {
            return Err(SemanticSurfaceError::DuplicateCanvasOrdinal(child.ordinal));
        }
    }
    let mut scoped = Vec::new();
    let mut materialized_visible = 0;
    let mut retained_offscreen = 0;
    let mut retained_selected = 0;
    let mut retained_focused = 0;
    for (ordinal, mut node) in ordered {
        let visible = policy.window.contains(ordinal);
        let retain = match policy.offscreen {
            CanvasOffscreenPolicy::Omit => false,
            CanvasOffscreenPolicy::RetainFocused => node.state.focused,
            CanvasOffscreenPolicy::RetainFocusedAndSelected => {
                node.state.focused || node.selection.selected
            }
        };
        if !visible && !retain {
            continue;
        }
        node.state.visibility = if visible {
            materialized_visible += 1;
            SemanticVisibility::Visible
        } else {
            retained_offscreen += 1;
            retained_selected += u64::from(node.selection.selected);
            retained_focused += u64::from(node.state.focused);
            SemanticVisibility::OffscreenRetained
        };
        node.selection.position_in_set = Some(ordinal + 1);
        node.selection.set_size = Some(policy.window.total);
        scoped.push(node);
    }
    let represented = materialized_visible + retained_offscreen;
    Ok(ScopedCanvasSemantics {
        children: scoped,
        summary: CanvasSemanticSummary {
            total: policy.window.total,
            requested_first: policy.window.first,
            requested_end: policy.window.visible_end(),
            materialized_visible,
            retained_offscreen,
            omitted: policy.window.total.saturating_sub(represented),
            retained_selected,
            retained_focused,
        },
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticSurfaceError {
    UnsupportedProjectionSchema {
        expected: u32,
        received: u32,
    },
    EmptySurfaceNodeKey(SemanticSurfaceNodeId),
    ForeignSurfaceNode {
        expected: WorkspaceViewId,
        node: SemanticSurfaceNodeId,
    },
    DuplicateSurfaceNode(SemanticSurfaceNodeId),
    DuplicateSurfaceAction {
        node: SemanticSurfaceNodeId,
        action: ActionId,
    },
    InvalidVisibleWindow(CanvasVisibleWindow),
    CanvasOrdinalOutOfRange {
        ordinal: u64,
        total: u64,
    },
    DuplicateCanvasOrdinal(u64),
    UnknownSurfaceNode(SemanticSurfaceNodeId),
    ActionNotExposed {
        node: SemanticSurfaceNodeId,
        action: ActionId,
    },
    SurfaceActionDisabled {
        node: SemanticSurfaceNodeId,
        action: ActionId,
        reason: Option<&'static str>,
    },
    StaleProjection {
        expected: SemanticProjectionStamp,
        received: SemanticProjectionStamp,
    },
}

impl fmt::Display for SemanticSurfaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProjectionSchema { expected, received } => write!(
                formatter,
                "semantic projection schema {received} is unsupported; expected {expected}"
            ),
            Self::EmptySurfaceNodeKey(node) => {
                write!(formatter, "semantic node {node:?} has an empty local key")
            }
            Self::ForeignSurfaceNode { expected, node } => write!(
                formatter,
                "semantic node {node:?} does not belong to workspace view {}",
                expected.0
            ),
            Self::DuplicateSurfaceNode(node) => {
                write!(formatter, "semantic node {node:?} occurs more than once")
            }
            Self::DuplicateSurfaceAction { node, action } => write!(
                formatter,
                "semantic node {node:?} exposes action {} more than once",
                action.0
            ),
            Self::InvalidVisibleWindow(window) => {
                write!(
                    formatter,
                    "canvas visible window {window:?} starts past its total"
                )
            }
            Self::CanvasOrdinalOutOfRange { ordinal, total } => write!(
                formatter,
                "canvas semantic ordinal {ordinal} is outside collection size {total}"
            ),
            Self::DuplicateCanvasOrdinal(ordinal) => {
                write!(formatter, "canvas semantic ordinal {ordinal} occurs twice")
            }
            Self::UnknownSurfaceNode(node) => {
                write!(formatter, "semantic surface node {node:?} is unknown")
            }
            Self::ActionNotExposed { node, action } => write!(
                formatter,
                "semantic node {node:?} does not expose action {}",
                action.0
            ),
            Self::SurfaceActionDisabled {
                node,
                action,
                reason,
            } => write!(
                formatter,
                "semantic action {} is disabled for {node:?}{}",
                action.0,
                reason.map_or(String::new(), |reason| format!(": {reason}"))
            ),
            Self::StaleProjection { expected, received } => write!(
                formatter,
                "semantic projection {received:?} is stale; current projection is {expected:?}"
            ),
        }
    }
}

impl Error for SemanticSurfaceError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceSemanticNodeId {
    Workspace,
    HiddenViews,
    HiddenTab(WorkspaceViewId),
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
    Reopen,
    Close,
    FloatOrDock,
    NextTab,
    PreviousTab,
    NextPane,
    PreviousPane,
}

/// Stable action IDs for workspace chrome. [`WorkspaceSemanticAction`] is the
/// typed lowering vocabulary accepted by the layout authority; these IDs are
/// the public identities that keyboard bindings, menus, pointer controls,
/// palettes and assistive technology share.
pub mod workspace_action_ids {
    use crate::ui_actions::ActionId;

    pub const FOCUS: ActionId = ActionId("audec.workspace.focus");
    pub const ACTIVATE: ActionId = ActionId("audec.workspace.activate");
    pub const REOPEN: ActionId = ActionId("audec.workspace.reopen");
    pub const CLOSE: ActionId = ActionId("audec.workspace.close");
    pub const FLOAT_OR_DOCK: ActionId = ActionId("audec.workspace.float_or_dock");
    pub const NEXT_TAB: ActionId = ActionId("audec.workspace.next_tab");
    pub const PREVIOUS_TAB: ActionId = ActionId("audec.workspace.previous_tab");
    pub const NEXT_PANE: ActionId = ActionId("audec.workspace.next_pane");
    pub const PREVIOUS_PANE: ActionId = ActionId("audec.workspace.previous_pane");
}

impl WorkspaceSemanticAction {
    pub const fn action_id(self) -> ActionId {
        use workspace_action_ids as ids;
        match self {
            Self::Focus => ids::FOCUS,
            Self::Activate => ids::ACTIVATE,
            Self::Reopen => ids::REOPEN,
            Self::Close => ids::CLOSE,
            Self::FloatOrDock => ids::FLOAT_OR_DOCK,
            Self::NextTab => ids::NEXT_TAB,
            Self::PreviousTab => ids::PREVIOUS_TAB,
            Self::NextPane => ids::NEXT_PANE,
            Self::PreviousPane => ids::PREVIOUS_PANE,
        }
    }

    pub fn from_action_id(id: ActionId) -> Option<Self> {
        use workspace_action_ids as ids;
        match id {
            ids::FOCUS => Some(Self::Focus),
            ids::ACTIVATE => Some(Self::Activate),
            ids::REOPEN => Some(Self::Reopen),
            ids::CLOSE => Some(Self::Close),
            ids::FLOAT_OR_DOCK => Some(Self::FloatOrDock),
            ids::NEXT_TAB => Some(Self::NextTab),
            ids::PREVIOUS_TAB => Some(Self::PreviousTab),
            ids::NEXT_PANE => Some(Self::NextPane),
            ids::PREVIOUS_PANE => Some(Self::PreviousPane),
            _ => None,
        }
    }
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
        let hidden = layout
            .document()
            .views
            .values()
            .filter(|descriptor| {
                matches!(
                    layout.document().location(descriptor.id),
                    Ok(ViewLocation::Hidden)
                )
            })
            .map(|descriptor| WorkspaceSemanticNode {
                id: WorkspaceSemanticNodeId::HiddenTab(descriptor.id),
                role: WorkspaceSemanticRole::Tab,
                label: descriptor
                    .title_override
                    .clone()
                    .unwrap_or_else(|| kind_label(&descriptor.kind).into()),
                state: WorkspaceSemanticState {
                    hidden: true,
                    ..Default::default()
                },
                actions: vec![
                    WorkspaceSemanticAction::Reopen,
                    WorkspaceSemanticAction::Activate,
                ],
                children: Vec::new(),
            })
            .collect::<Vec<_>>();
        if !hidden.is_empty() {
            windows.push(WorkspaceSemanticNode {
                id: WorkspaceSemanticNodeId::HiddenViews,
                role: WorkspaceSemanticRole::TabList,
                label: "Closed views".into(),
                state: WorkspaceSemanticState::default(),
                actions: Vec::new(),
                children: hidden,
            });
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
                            WorkspaceSemanticAction::NextTab,
                            WorkspaceSemanticAction::PreviousTab,
                            WorkspaceSemanticAction::NextPane,
                            WorkspaceSemanticAction::PreviousPane,
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
    let tree = WorkspaceSemanticTree::from_layout(layout);
    let semantic = tree
        .find(node)
        .ok_or(WorkspaceSemanticError::UnknownNode(node))?;
    if !semantic.actions.contains(&action) {
        return Err(WorkspaceSemanticError::UnsupportedAction { node, action });
    }
    if semantic.state.disabled {
        return Err(WorkspaceSemanticError::ActionDisabled { node, action });
    }
    match action {
        WorkspaceSemanticAction::Focus | WorkspaceSemanticAction::Activate => match node {
            WorkspaceSemanticNodeId::HiddenTab(view) => {
                Ok(WorkspaceLayoutCommand::ReopenTab(PaneInstanceId(view)))
            }
            _ => Ok(WorkspaceLayoutCommand::FocusPane(PaneInstanceId(
                view_for_node(node).ok_or(WorkspaceSemanticError::NodeHasNoPane(node))?,
            ))),
        },
        WorkspaceSemanticAction::Reopen => {
            let WorkspaceSemanticNodeId::HiddenTab(view) = node else {
                return Err(WorkspaceSemanticError::NodeHasNoPane(node));
            };
            Ok(WorkspaceLayoutCommand::ReopenTab(PaneInstanceId(view)))
        }
        WorkspaceSemanticAction::Close => {
            let pane = view_for_node(node).ok_or(WorkspaceSemanticError::NodeHasNoPane(node))?;
            if layout.document().views[&pane].kind.close_behavior() == CloseBehavior::Pinned {
                return Err(WorkspaceSemanticError::ActionDisabled { node, action });
            }
            Ok(WorkspaceLayoutCommand::CloseTab(PaneInstanceId(pane)))
        }
        WorkspaceSemanticAction::FloatOrDock => {
            let pane = view_for_node(node).ok_or(WorkspaceSemanticError::NodeHasNoPane(node))?;
            let placement = layout
                .placement(PaneInstanceId(pane))
                .ok_or(WorkspaceSemanticError::PaneHidden(pane))?;
            if matches!(placement.window, WorkspaceWindow::Main)
                && !layout.document().views[&pane].kind.can_float()
            {
                return Err(WorkspaceSemanticError::ActionDisabled { node, action });
            }
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
            let pane = pane_view_for_navigation(layout, node, action)
                .ok_or(WorkspaceSemanticError::NodeHasNoPane(node))?;
            Ok(WorkspaceLayoutCommand::FocusPane(PaneInstanceId(pane)))
        }
    }
}

/// Action-ID entry point used by menu, palette, pointer and accessibility
/// adapters. It lowers into the existing typed workspace command only after
/// confirming that the node advertises the requested action.
pub fn command_for_workspace_action_id(
    layout: &WorkspaceSessionLayout,
    node: WorkspaceSemanticNodeId,
    action: ActionId,
) -> Result<WorkspaceLayoutCommand, WorkspaceSemanticError> {
    let semantic = WorkspaceSemanticAction::from_action_id(action)
        .ok_or(WorkspaceSemanticError::UnknownActionId(action))?;
    command_for_semantic_action(layout, node, semantic)
}

fn view_for_node(node: WorkspaceSemanticNodeId) -> Option<WorkspaceViewId> {
    match node {
        WorkspaceSemanticNodeId::Tab(view)
        | WorkspaceSemanticNodeId::HiddenTab(view)
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
    let (window, pane_id) = match node {
        WorkspaceSemanticNodeId::Pane(pane) | WorkspaceSemanticNodeId::TabList(pane) => {
            window_for_dock_pane(layout, pane).map(|window| (window, pane))?
        }
        WorkspaceSemanticNodeId::Window(window)
        | WorkspaceSemanticNodeId::SplitGroup { window, .. } => {
            let pane = layout.focused_pane(window)?;
            (window, layout.placement(pane)?.dock_pane)
        }
        WorkspaceSemanticNodeId::Tab(view)
        | WorkspaceSemanticNodeId::Content(view)
        | WorkspaceSemanticNodeId::CloseControl(view)
        | WorkspaceSemanticNodeId::FloatDockControl(view) => {
            let placement = layout.placement(PaneInstanceId(view))?;
            (placement.window, placement.dock_pane)
        }
        WorkspaceSemanticNodeId::Workspace => {
            let pane = layout.focused_pane(WorkspaceWindow::Main)?;
            (WorkspaceWindow::Main, layout.placement(pane)?.dock_pane)
        }
        WorkspaceSemanticNodeId::HiddenViews | WorkspaceSemanticNodeId::HiddenTab(_) => {
            return None;
        }
    };
    let window_layout = match window {
        WorkspaceWindow::Main => &layout.document().main_layout,
        WorkspaceWindow::Floating(window) => {
            &layout.document().floating_windows.get(&window)?.layout
        }
    };
    let mut panes = Vec::new();
    collect_panes(window_layout, &mut panes);
    match action {
        WorkspaceSemanticAction::NextTab | WorkspaceSemanticAction::PreviousTab => {
            let (_, items, active) = panes.iter().find(|(pane, _, _)| *pane == pane_id)?;
            let next = if action == WorkspaceSemanticAction::NextTab {
                (active + 1) % items.len()
            } else {
                (active + items.len() - 1) % items.len()
            };
            items.get(next).copied()
        }
        WorkspaceSemanticAction::NextPane | WorkspaceSemanticAction::PreviousPane => {
            let index = panes
                .iter()
                .position(|(candidate, _, _)| *candidate == pane_id)?;
            let next = if action == WorkspaceSemanticAction::NextPane {
                (index + 1) % panes.len()
            } else {
                (index + panes.len() - 1) % panes.len()
            };
            let (_, items, active) = &panes[next];
            items.get(*active).copied()
        }
        _ => None,
    }
}

fn collect_panes(layout: &DockLayout, panes: &mut Vec<(DockPaneId, Vec<WorkspaceViewId>, usize)>) {
    match layout {
        DockLayout::Pane {
            pane_id,
            items,
            active,
            ..
        } => {
            panes.push((*pane_id, items.clone(), *active));
        }
        DockLayout::Split { first, second, .. } => {
            collect_panes(first, panes);
            collect_panes(second, panes);
        }
    }
}

fn window_for_dock_pane(
    layout: &WorkspaceSessionLayout,
    pane: DockPaneId,
) -> Option<WorkspaceWindow> {
    let mut panes = Vec::new();
    collect_panes(&layout.document().main_layout, &mut panes);
    if panes.iter().any(|(candidate, _, _)| *candidate == pane) {
        return Some(WorkspaceWindow::Main);
    }
    layout
        .document()
        .floating_windows
        .iter()
        .find_map(|(&window, floating)| {
            panes.clear();
            collect_panes(&floating.layout, &mut panes);
            panes
                .iter()
                .any(|(candidate, _, _)| *candidate == pane)
                .then_some(WorkspaceWindow::Floating(window))
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSemanticError {
    UnknownActionId(ActionId),
    UnknownNode(WorkspaceSemanticNodeId),
    NodeHasNoPane(WorkspaceSemanticNodeId),
    PaneHidden(WorkspaceViewId),
    UnsupportedAction {
        node: WorkspaceSemanticNodeId,
        action: WorkspaceSemanticAction,
    },
    ActionDisabled {
        node: WorkspaceSemanticNodeId,
        action: WorkspaceSemanticAction,
    },
}

impl fmt::Display for WorkspaceSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownActionId(action) => {
                write!(formatter, "workspace action ID {} is unknown", action.0)
            }
            Self::UnknownNode(node) => {
                write!(formatter, "semantic workspace node {node:?} is unknown")
            }
            Self::NodeHasNoPane(node) => write!(
                formatter,
                "semantic workspace node {node:?} has no pane action"
            ),
            Self::PaneHidden(view) => write!(formatter, "workspace view {} is hidden", view.0),
            Self::UnsupportedAction { node, action } => write!(
                formatter,
                "semantic workspace node {node:?} does not support {action:?}"
            ),
            Self::ActionDisabled { node, action } => write!(
                formatter,
                "semantic workspace action {action:?} is disabled for {node:?}"
            ),
        }
    }
}

impl Error for WorkspaceSemanticError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_session::ProjectSessionId;
    use crate::ui_actions::ActionRegistry;
    use crate::workspace_document::{LegacyBuiltinView, WorkspaceDocument};

    fn layout() -> WorkspaceSessionLayout {
        WorkspaceSessionLayout::from_document(ProjectSessionId(51), WorkspaceDocument::default())
            .unwrap()
    }

    fn clip_node(
        view: WorkspaceViewId,
        clip: u64,
        registry: &ActionRegistry,
    ) -> SemanticSurfaceNode {
        let mut node = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::object(view, "clip", clip),
            SemanticSurfaceRole::Clip,
            format!("Audio clip {clip}"),
        );
        node.actions.push(
            SemanticActionBinding::resolve(
                registry,
                ActionId("audec.edit.delete"),
                &ActionContext {
                    has_project: true,
                    has_selection: true,
                    ..ActionContext::default()
                },
            )
            .unwrap(),
        );
        node
    }

    fn clip_surface(view: WorkspaceViewId, revision: u64) -> SemanticSurface {
        let registry = ActionRegistry::audec_defaults();
        let mut root = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::root(view),
            SemanticSurfaceRole::Timeline,
            "Arrangement timeline",
        );
        root.children.push(clip_node(view, 41, &registry));
        SemanticSurface::new(view, revision, root).unwrap()
    }

    #[test]
    fn semantic_object_identity_survives_reordering_and_reprojection() {
        let view = LegacyBuiltinView::Track.id();
        let registry = ActionRegistry::audec_defaults();
        let stable = SemanticSurfaceNodeId::object(view, "clip", 41);

        let mut first_root = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::root(view),
            SemanticSurfaceRole::Timeline,
            "Arrangement",
        );
        first_root.children = vec![
            clip_node(view, 41, &registry),
            clip_node(view, 99, &registry),
        ];
        let first = SemanticSurface::new(view, 5, first_root).unwrap().project();

        let mut second_root = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::root(view),
            SemanticSurfaceRole::Timeline,
            "Renamed arrangement",
        );
        second_root.children = vec![
            clip_node(view, 99, &registry),
            clip_node(view, 41, &registry),
        ];
        let second = SemanticSurface::new(view, 6, second_root)
            .unwrap()
            .project();

        assert!(first.nodes.iter().any(|node| node.id == stable));
        assert!(second.nodes.iter().any(|node| node.id == stable));
    }

    #[test]
    fn canvas_projection_contains_only_the_scoped_visible_window() {
        let view = LegacyBuiltinView::Track.id();
        let registry = ActionRegistry::audec_defaults();
        let children = [5, 3, 0, 2, 4, 1].map(|ordinal| CanvasSemanticChild {
            ordinal,
            node: clip_node(view, 100 + ordinal, &registry),
        });
        let scoped = scope_canvas_semantic_children(
            children,
            CanvasSemanticPolicy {
                window: CanvasVisibleWindow {
                    first: 2,
                    count: 2,
                    total: 6,
                },
                offscreen: CanvasOffscreenPolicy::Omit,
            },
        )
        .unwrap();

        assert_eq!(
            scoped
                .iter()
                .map(|node| node.id.clone())
                .collect::<Vec<_>>(),
            vec![
                SemanticSurfaceNodeId::object(view, "clip", 102),
                SemanticSurfaceNodeId::object(view, "clip", 103),
            ]
        );
        assert_eq!(scoped[0].selection.position_in_set, Some(3));
        assert_eq!(scoped[0].selection.set_size, Some(6));
        assert_eq!(scoped[0].state.visibility, SemanticVisibility::Visible);
    }

    #[test]
    fn canvas_scope_reports_omitted_and_retained_objects_without_claiming_visibility() {
        let view = LegacyBuiltinView::Track.id();
        let registry = ActionRegistry::audec_defaults();
        let children = (0..8).map(|ordinal| {
            let mut node = clip_node(view, 100 + ordinal, &registry);
            if ordinal == 0 {
                node.selection.selected = true;
            }
            CanvasSemanticChild { ordinal, node }
        });
        let scoped = scope_canvas_semantics(
            children,
            CanvasSemanticPolicy {
                window: CanvasVisibleWindow {
                    first: 3,
                    count: 2,
                    total: 8,
                },
                offscreen: CanvasOffscreenPolicy::RetainFocusedAndSelected,
            },
        )
        .unwrap();

        assert_eq!(scoped.summary.materialized_visible, 2);
        assert_eq!(scoped.summary.retained_offscreen, 1);
        assert_eq!(scoped.summary.retained_selected, 1);
        assert_eq!(scoped.summary.omitted, 5);
        assert_eq!(
            scoped.children[0].state.visibility,
            SemanticVisibility::OffscreenRetained
        );
        assert!(scoped
            .summary
            .announcement()
            .contains("1 offscreen items retained outside native traversal"));
    }

    #[test]
    fn offscreen_retention_never_claims_that_a_selected_node_is_visible() {
        let view = LegacyBuiltinView::Track.id();
        let registry = ActionRegistry::audec_defaults();
        let mut offscreen = clip_node(view, 100, &registry);
        offscreen.selection.selected = true;
        let scoped = scope_canvas_semantic_children(
            [CanvasSemanticChild {
                ordinal: 0,
                node: offscreen,
            }],
            CanvasSemanticPolicy {
                window: CanvasVisibleWindow {
                    first: 5,
                    count: 2,
                    total: 8,
                },
                offscreen: CanvasOffscreenPolicy::RetainFocusedAndSelected,
            },
        )
        .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(
            scoped[0].state.visibility,
            SemanticVisibility::OffscreenRetained
        );
    }

    #[test]
    fn every_input_origin_routes_the_same_registry_action_id() {
        let view = LegacyBuiltinView::Track.id();
        let surface = clip_surface(view, 8);
        let node = SemanticSurfaceNodeId::object(view, "clip", 41);
        for origin in [
            SemanticActionOrigin::Keyboard,
            SemanticActionOrigin::Menu,
            SemanticActionOrigin::ContextMenu,
            SemanticActionOrigin::Palette,
            SemanticActionOrigin::Pointer,
            SemanticActionOrigin::AssistiveTechnology,
        ] {
            let dispatch = surface
                .route_action(&SemanticActionRequest {
                    projection: surface.stamp,
                    node: node.clone(),
                    action: ActionId("audec.edit.delete"),
                    origin,
                    modifiers: InvocationModifiers::default(),
                })
                .unwrap();
            assert_eq!(dispatch.action, ActionId("audec.edit.delete"));
            assert_eq!(dispatch.origin, origin);
            assert_eq!(dispatch.view, Some(view));
        }
    }

    #[test]
    fn action_from_a_stale_projection_is_rejected_before_node_lookup() {
        let view = LegacyBuiltinView::Track.id();
        let surface = clip_surface(view, 9);
        let error = surface
            .route_action(&SemanticActionRequest {
                projection: SemanticProjectionStamp { view, revision: 8 },
                node: SemanticSurfaceNodeId::object(view, "clip", 41),
                action: ActionId("audec.edit.delete"),
                origin: SemanticActionOrigin::AssistiveTechnology,
                modifiers: InvocationModifiers::default(),
            })
            .unwrap_err();
        assert!(matches!(
            error,
            SemanticSurfaceError::StaleProjection {
                expected: SemanticProjectionStamp { revision: 9, .. },
                received: SemanticProjectionStamp { revision: 8, .. },
            }
        ));
    }

    #[test]
    fn delayed_projection_cannot_replace_a_newer_native_tree() {
        let view = LegacyBuiltinView::Track.id();
        let mut coordinator = SemanticProjectionCoordinator::default();
        let newest = clip_surface(view, 11).project();
        let delayed = clip_surface(view, 10).project();

        coordinator.replace(&newest).unwrap();
        let error = coordinator.replace(&delayed).unwrap_err();

        assert!(matches!(
            error,
            SemanticSurfaceError::StaleProjection {
                expected: SemanticProjectionStamp { revision: 11, .. },
                received: SemanticProjectionStamp { revision: 10, .. },
            }
        ));
        assert_eq!(coordinator.installed(view), Some(newest.stamp));
    }

    #[test]
    fn workspace_chrome_lowers_action_ids_through_the_typed_authority() {
        let layout = layout();
        let pane = layout.document().main_layout.primary_pane();
        let from_id = command_for_workspace_action_id(
            &layout,
            WorkspaceSemanticNodeId::TabList(pane),
            workspace_action_ids::PREVIOUS_TAB,
        )
        .unwrap();
        let typed = command_for_semantic_action(
            &layout,
            WorkspaceSemanticNodeId::TabList(pane),
            WorkspaceSemanticAction::PreviousTab,
        )
        .unwrap();
        assert_eq!(from_id, typed);
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
    fn tab_navigation_uses_the_tab_context_instead_of_refocusing_itself() {
        let layout = layout();
        let pane = layout.document().main_layout.primary_pane();
        let (first, second) = match &layout.document().main_layout {
            DockLayout::Pane { items, .. } => (items[0], items[1]),
            DockLayout::Split { first, .. } => match first.as_ref() {
                DockLayout::Pane { items, .. } => (items[0], items[1]),
                DockLayout::Split { .. } => panic!("unexpected nested default layout"),
            },
        };
        let command = command_for_semantic_action(
            &layout,
            WorkspaceSemanticNodeId::Tab(first),
            WorkspaceSemanticAction::NextTab,
        )
        .unwrap();
        assert!(matches!(
            command,
            WorkspaceLayoutCommand::FocusPane(PaneInstanceId(actual)) if actual == second
        ));
        assert_eq!(
            layout.placement(PaneInstanceId(first)).unwrap().dock_pane,
            pane
        );
    }

    #[test]
    fn pane_navigation_never_leaks_out_of_its_native_window() {
        let mut layout = layout();
        let pane = PaneInstanceId(LegacyBuiltinView::Waterfall.id());
        let transition = layout.tear_off_pane(pane, None).unwrap();
        let window = transition
            .windows
            .iter()
            .find_map(|effect| match effect {
                crate::workspace_session_layout::NativeWindowEffect::Open { window, .. } => {
                    Some(*window)
                }
                _ => None,
            })
            .unwrap();
        let command = command_for_semantic_action(
            &layout,
            WorkspaceSemanticNodeId::Window(WorkspaceWindow::Floating(window)),
            WorkspaceSemanticAction::NextPane,
        )
        .unwrap();
        assert!(matches!(
            command,
            WorkspaceLayoutCommand::FocusPane(actual) if actual == pane
        ));
    }

    #[test]
    fn hidden_views_are_discoverable_and_reopenable() {
        let mut layout = layout();
        let pane = PaneInstanceId(LegacyBuiltinView::Rhythm.id());
        layout.close_tab(pane).unwrap();
        let tree = WorkspaceSemanticTree::from_layout(&layout);
        let hidden = tree
            .find(WorkspaceSemanticNodeId::HiddenTab(pane.0))
            .expect("closed view remains in semantic tree");
        assert!(hidden.state.hidden);
        assert!(matches!(
            command_for_semantic_action(
                &layout,
                hidden.id,
                WorkspaceSemanticAction::Reopen,
            )
            .unwrap(),
            WorkspaceLayoutCommand::ReopenTab(actual) if actual == pane
        ));
    }

    #[test]
    fn disabled_and_unadvertised_controls_do_not_route_commands() {
        let layout = layout();
        let overview = LegacyBuiltinView::Track.id();
        assert!(matches!(
            command_for_semantic_action(
                &layout,
                WorkspaceSemanticNodeId::CloseControl(overview),
                WorkspaceSemanticAction::Close,
            ),
            Err(WorkspaceSemanticError::ActionDisabled { .. })
        ));
        assert!(matches!(
            command_for_semantic_action(
                &layout,
                WorkspaceSemanticNodeId::Content(overview),
                WorkspaceSemanticAction::Close,
            ),
            Err(WorkspaceSemanticError::UnsupportedAction { .. })
        ));
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
