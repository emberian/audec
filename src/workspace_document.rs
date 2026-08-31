//! Durable, toolkit-free workspace document model.
//!
//! A workspace is a description of *views of* a project, never a second copy
//! of the project.  This module owns stable view/window/link identities,
//! descriptors, placement and portable JSON.  It deliberately does not know
//! about GPUI entities, Guise item IDs, native window handles, or any renderer
//! factory.  An application adapter resolves the typed target references and
//! maps `WorkspaceViewId` to ephemeral runtime handles.
//!
//! The v2 document supersedes the six fixed Guise-item slots conceptually. It
//! reserves their durable IDs for migration, but permits arbitrarily many
//! descriptor instances thereafter.  Project-domain references are stored in
//! their named domains rather than pretending that equal `u64`s from different
//! domains are interchangeable identities.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const WORKSPACE_DOCUMENT_VERSION: u32 = 2;

/// Stable persisted identity of one view instance. Zero is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceViewId(pub u64);

impl WorkspaceViewId {
    pub const TRACK_OVERVIEW: Self = Self(1);
    pub const WATERFALL: Self = Self(2);
    pub const RHYTHM: Self = Self(3);
    pub const COMPONENTS: Self = Self(4);
    pub const SEPARATION: Self = Self(5);
    pub const LOOM: Self = Self(6);
    pub const FIRST_DYNAMIC: u64 = 7;
}

/// Stable persisted identity of a native floating window. Zero is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceWindowId(pub u64);

/// Stable persisted identity of a group which may link view-local state.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LinkGroupId(pub u64);

impl LinkGroupId {
    /// An unlinked view does not broadcast or receive any view-local state.
    pub const UNLINKED: Self = Self(0);
}

/// Stable persisted identity of a dock-tree pane. It is not a runtime pane ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DockPaneId(pub u64);

/// The six singleton descriptors available before dynamic workspace items.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyBuiltinView {
    Track,
    Waterfall,
    Rhythm,
    Components,
    Separation,
    Loom,
}

impl LegacyBuiltinView {
    pub const ALL: [Self; 6] = [
        Self::Track,
        Self::Waterfall,
        Self::Rhythm,
        Self::Components,
        Self::Separation,
        Self::Loom,
    ];

    pub const fn id(self) -> WorkspaceViewId {
        match self {
            Self::Track => WorkspaceViewId::TRACK_OVERVIEW,
            Self::Waterfall => WorkspaceViewId::WATERFALL,
            Self::Rhythm => WorkspaceViewId::RHYTHM,
            Self::Components => WorkspaceViewId::COMPONENTS,
            Self::Separation => WorkspaceViewId::SEPARATION,
            Self::Loom => WorkspaceViewId::LOOM,
        }
    }
}

/// Which editor/lens factory an application should use for a descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceItemKind {
    Overview,
    Arrangement,
    Browser,
    Inspector,
    PatternEditor {
        mode: PatternEditorMode,
    },
    AutomationEditor,
    Mixer,
    AnalysisLens {
        lens: AnalysisLensKind,
    },
    Render,
    /// An extension is explicitly named and opaque to the portable document.
    Extension {
        namespace: String,
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternEditorMode {
    PianoRoll,
    Steps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisLensKind {
    Waveform,
    Spectrum,
    Waterfall,
    Rhythm,
    Components,
    Separation,
    Loom,
    Coverage,
    Comparison,
    AirQuery,
}

/// A target names a document object but does not resolve it. Resolution belongs
/// to the bridge/application against one revisioned project snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditorTarget {
    Project,
    Arrangement,
    Assets,
    Inspector,
    PatternDefinition { id: u64 },
    AutomationLane { id: u64 },
    Mixer { bus_id: Option<u64> },
    Analysis { source_id: Option<u64> },
    Explanation { proposal_id: u64 },
    Render { comparison_id: Option<u64> },
    Extension { namespace: String, key: String },
}

/// Lifecycle policy is a descriptor fact, not a GPUI close callback policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseBehavior {
    Pinned,
    Hide,
    RemoveDescriptor,
}

impl WorkspaceItemKind {
    pub const fn close_behavior(&self) -> CloseBehavior {
        match self {
            Self::Overview => CloseBehavior::Pinned,
            Self::Browser
            | Self::Inspector
            | Self::Arrangement
            | Self::PatternEditor { .. }
            | Self::AutomationEditor
            | Self::Mixer => CloseBehavior::Hide,
            Self::AnalysisLens { .. } | Self::Render | Self::Extension { .. } => {
                CloseBehavior::RemoveDescriptor
            }
        }
    }

    pub const fn can_float(&self) -> bool {
        !matches!(self, Self::Overview)
    }
}

/// Which view-local facts are eligible for optional synchronized navigation.
/// Transport is deliberately absent: it is global project state, not a view
/// link facet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LinkFacets(pub u16);

impl LinkFacets {
    pub const NONE: Self = Self(0);
    pub const TIME: Self = Self(1 << 0);
    pub const FREQUENCY: Self = Self(1 << 1);
    pub const SELECTION: Self = Self(1 << 2);
    pub const EDIT_CURSOR: Self = Self(1 << 3);
    pub const FOLLOW: Self = Self(1 << 4);
    pub const ALL: Self = Self(
        Self::TIME.0 | Self::FREQUENCY.0 | Self::SELECTION.0 | Self::EDIT_CURSOR.0 | Self::FOLLOW.0,
    );

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewLinkMembership {
    pub group: LinkGroupId,
    pub facets: LinkFacets,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkGroupDescriptor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameViewport {
    pub start: i64,
    pub end: i64,
}

impl FrameViewport {
    pub fn validate(self) -> Result<(), WorkspaceDocumentError> {
        if self.start >= self.end {
            return Err(WorkspaceDocumentError::InvalidViewState(
                "frame viewport must be non-empty",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeatViewport {
    pub start_tick: i64,
    pub end_tick: i64,
}

impl BeatViewport {
    pub fn validate(self) -> Result<(), WorkspaceDocumentError> {
        if self.start_tick >= self.end_tick {
            return Err(WorkspaceDocumentError::InvalidViewState(
                "beat viewport must be non-empty",
            ));
        }
        Ok(())
    }
}

/// Common state is structured so the document can validate basic coordinate
/// invariants. `extensions` is reserved for kind-specific state that this core
/// does not own yet, and is intentionally round-tripped unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditorViewState {
    Overview {
        viewport: FrameViewport,
        follow: bool,
    },
    Arrangement {
        viewport: FrameViewport,
        follow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header_width: Option<f32>,
    },
    Browser {
        #[serde(default)]
        search: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_asset_id: Option<u64>,
    },
    Inspector,
    Pattern {
        viewport: BeatViewport,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vertical_origin: Option<i32>,
    },
    Automation {
        viewport: BeatViewport,
    },
    Mixer,
    Analysis {
        viewport: FrameViewport,
        follow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_frequency_hz: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_frequency_hz: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recipe_fingerprint: Option<String>,
    },
    Render,
    Extension {
        #[serde(default)]
        data: Value,
    },
}

impl EditorViewState {
    fn validate(&self) -> Result<(), WorkspaceDocumentError> {
        match self {
            Self::Overview { viewport, .. }
            | Self::Arrangement { viewport, .. }
            | Self::Analysis { viewport, .. } => viewport.validate(),
            Self::Pattern { viewport, .. } | Self::Automation { viewport } => viewport.validate(),
            _ => Ok(()),
        }?;

        if let Self::Arrangement {
            header_width: Some(width),
            ..
        } = self
        {
            if !width.is_finite() || *width <= 0.0 {
                return Err(WorkspaceDocumentError::InvalidViewState(
                    "arrangement header width must be finite and positive",
                ));
            }
        }
        if let Self::Analysis {
            min_frequency_hz,
            max_frequency_hz,
            ..
        } = self
        {
            if let Some(value) = min_frequency_hz {
                if !value.is_finite() || *value < 0.0 {
                    return Err(WorkspaceDocumentError::InvalidViewState(
                        "analysis minimum frequency must be finite and non-negative",
                    ));
                }
            }
            if let Some(value) = max_frequency_hz {
                if !value.is_finite() || *value <= 0.0 {
                    return Err(WorkspaceDocumentError::InvalidViewState(
                        "analysis maximum frequency must be finite and positive",
                    ));
                }
            }
            if let (Some(minimum), Some(maximum)) = (min_frequency_hz, max_frequency_hz) {
                if minimum >= maximum {
                    return Err(WorkspaceDocumentError::InvalidViewState(
                        "analysis frequency range must be ordered",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A persistent editor/lens instance. Its `state` is navigation/presentation
/// state only; musical, audio, mixer, and AIR truth remain in the project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceViewDescriptor {
    pub id: WorkspaceViewId,
    pub kind: WorkspaceItemKind,
    pub target: EditorTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_override: Option<String>,
    #[serde(default)]
    pub links: ViewLinkMembership,
    pub state: EditorViewState,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl WorkspaceViewDescriptor {
    pub fn validate(&self) -> Result<(), WorkspaceDocumentError> {
        if self.id.0 == 0 {
            return Err(WorkspaceDocumentError::ZeroViewId);
        }
        if self
            .title_override
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(WorkspaceDocumentError::EmptyTitle(self.id));
        }
        if !kind_accepts_target(&self.kind, &self.target)
            || !kind_accepts_state(&self.kind, &self.state)
        {
            return Err(WorkspaceDocumentError::DescriptorMismatch(self.id));
        }
        if self.links.group == LinkGroupId::UNLINKED && self.links.facets != LinkFacets::NONE {
            return Err(WorkspaceDocumentError::UnlinkedFacets(self.id));
        }
        self.state.validate()
    }
}

fn kind_accepts_target(kind: &WorkspaceItemKind, target: &EditorTarget) -> bool {
    matches!(
        (kind, target),
        (WorkspaceItemKind::Overview, EditorTarget::Project)
            | (WorkspaceItemKind::Arrangement, EditorTarget::Arrangement)
            | (WorkspaceItemKind::Browser, EditorTarget::Assets)
            | (WorkspaceItemKind::Inspector, EditorTarget::Inspector)
            | (
                WorkspaceItemKind::PatternEditor { .. },
                EditorTarget::PatternDefinition { .. }
            )
            | (
                WorkspaceItemKind::AutomationEditor,
                EditorTarget::AutomationLane { .. }
            )
            | (WorkspaceItemKind::Mixer, EditorTarget::Mixer { .. })
            | (
                WorkspaceItemKind::AnalysisLens { .. },
                EditorTarget::Analysis { .. }
            )
            | (
                WorkspaceItemKind::AnalysisLens { .. },
                EditorTarget::Explanation { .. }
            )
            | (WorkspaceItemKind::Render, EditorTarget::Render { .. })
            | (
                WorkspaceItemKind::Extension { .. },
                EditorTarget::Extension { .. }
            )
    )
}

fn kind_accepts_state(kind: &WorkspaceItemKind, state: &EditorViewState) -> bool {
    matches!(
        (kind, state),
        (
            WorkspaceItemKind::Overview,
            EditorViewState::Overview { .. }
        ) | (
            WorkspaceItemKind::Arrangement,
            EditorViewState::Arrangement { .. }
        ) | (WorkspaceItemKind::Browser, EditorViewState::Browser { .. })
            | (WorkspaceItemKind::Inspector, EditorViewState::Inspector)
            | (
                WorkspaceItemKind::PatternEditor { .. },
                EditorViewState::Pattern { .. }
            )
            | (
                WorkspaceItemKind::AutomationEditor,
                EditorViewState::Automation { .. }
            )
            | (WorkspaceItemKind::Mixer, EditorViewState::Mixer)
            | (
                WorkspaceItemKind::AnalysisLens { .. },
                EditorViewState::Analysis { .. }
            )
            | (WorkspaceItemKind::Render, EditorViewState::Render)
            | (
                WorkspaceItemKind::Extension { .. },
                EditorViewState::Extension { .. }
            )
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// Toolkit-neutral tab/split tree. Every placed view appears exactly once over
/// the main tree and all floating-window trees; hidden descriptors are absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DockLayout {
    Pane {
        pane_id: DockPaneId,
        items: Vec<WorkspaceViewId>,
        active: usize,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<DockLayout>,
        second: Box<DockLayout>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
}

impl DockLayout {
    pub fn primary_pane(&self) -> DockPaneId {
        match self {
            Self::Pane { pane_id, .. } => *pane_id,
            Self::Split { first, .. } => first.primary_pane(),
        }
    }

    fn validate_into(
        &self,
        known_views: &BTreeMap<WorkspaceViewId, WorkspaceViewDescriptor>,
        seen_views: &mut BTreeSet<WorkspaceViewId>,
        seen_panes: &mut BTreeSet<DockPaneId>,
    ) -> Result<(), WorkspaceDocumentError> {
        match self {
            Self::Pane {
                pane_id,
                items,
                active,
                ..
            } => {
                if pane_id.0 == 0 || !seen_panes.insert(*pane_id) {
                    return Err(WorkspaceDocumentError::DuplicatePane(*pane_id));
                }
                if items.is_empty() || *active >= items.len() {
                    return Err(WorkspaceDocumentError::InvalidLayout(
                        "a pane must contain an active item",
                    ));
                }
                for view in items {
                    if !known_views.contains_key(view) {
                        return Err(WorkspaceDocumentError::UnknownView(*view));
                    }
                    if !seen_views.insert(*view) {
                        return Err(WorkspaceDocumentError::DuplicatePlacement(*view));
                    }
                }
                Ok(())
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || *ratio <= 0.0 || *ratio >= 1.0 {
                    return Err(WorkspaceDocumentError::InvalidLayout(
                        "split ratio must be finite and inside (0, 1)",
                    ));
                }
                first.validate_into(known_views, seen_views, seen_panes)?;
                second.validate_into(known_views, seen_views, seen_panes)
            }
        }
    }

    fn without(self, view: WorkspaceViewId) -> (Option<Self>, bool) {
        match self {
            Self::Pane {
                pane_id,
                mut items,
                mut active,
                extensions,
            } => {
                let Some(index) = items.iter().position(|item| *item == view) else {
                    return (
                        Some(Self::Pane {
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
                    Some(Self::Pane {
                        pane_id,
                        items,
                        active,
                        extensions,
                    }),
                    true,
                )
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
                extensions,
            } => {
                let (first, first_removed) = first.without(view);
                let (second, second_removed) = second.without(view);
                let next = match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
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

    fn add_to_primary(&mut self, view: WorkspaceViewId) {
        match self {
            Self::Pane { items, active, .. } => {
                items.push(view);
                *active = items.len() - 1;
            }
            Self::Split { first, .. } => first.add_to_primary(view),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Windowed,
    Maximized,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub mode: WindowMode,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowPlacement {
    pub fn validate(self) -> Result<(), WorkspaceDocumentError> {
        if ![self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f32::is_finite)
            || self.width <= 0.0
            || self.height <= 0.0
        {
            return Err(WorkspaceDocumentError::InvalidWindowPlacement);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FloatingWindowDescriptor {
    pub id: WorkspaceWindowId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<WindowPlacement>,
    pub layout: DockLayout,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

/// Serialized allocators make newly created identities monotonic and make
/// deleted descriptor IDs permanently unavailable inside a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceIdAllocators {
    pub next_view: u64,
    pub next_window: u64,
    pub next_link_group: u64,
}

impl Default for WorkspaceIdAllocators {
    fn default() -> Self {
        Self {
            next_view: WorkspaceViewId::FIRST_DYNAMIC,
            next_window: 1,
            next_link_group: 1,
        }
    }
}

/// A fresh description allocated by [`WorkspaceDocument::create_view`].
#[derive(Clone, Debug, PartialEq)]
pub struct NewWorkspaceView {
    pub kind: WorkspaceItemKind,
    pub target: EditorTarget,
    pub title_override: Option<String>,
    pub links: ViewLinkMembership,
    pub state: EditorViewState,
    pub extensions: BTreeMap<String, Value>,
}

/// Versioned portable workspace document. `unknown_fields` is flattened so a
/// newer producer's top-level additions survive a read/save cycle unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceDocument {
    pub version: u32,
    pub allocators: WorkspaceIdAllocators,
    pub views: BTreeMap<WorkspaceViewId, WorkspaceViewDescriptor>,
    pub main_layout: DockLayout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_window: Option<WindowPlacement>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub floating_windows: BTreeMap<WorkspaceWindowId, FloatingWindowDescriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub link_groups: BTreeMap<LinkGroupId, LinkGroupDescriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub unknown_fields: BTreeMap<String, Value>,
}

impl Default for WorkspaceDocument {
    fn default() -> Self {
        Self::from_legacy_six(LegacySixWorkspace::default()).expect("default legacy workspace")
    }
}

impl WorkspaceDocument {
    pub fn from_json(source: &str) -> Result<Self, WorkspaceDocumentError> {
        let document: Self = serde_json::from_str(source)
            .map_err(|error| WorkspaceDocumentError::Json(error.to_string()))?;
        document.validate()?;
        Ok(document)
    }

    pub fn to_json_pretty(&self) -> Result<String, WorkspaceDocumentError> {
        self.validate()?;
        serde_json::to_string_pretty(self)
            .map_err(|error| WorkspaceDocumentError::Json(error.to_string()))
    }

    pub fn from_legacy_six(legacy: LegacySixWorkspace) -> Result<Self, WorkspaceDocumentError> {
        legacy.validate()?;
        let legacy_group = LinkGroupId(1);
        let mut views = BTreeMap::new();
        for builtin in LegacyBuiltinView::ALL {
            let descriptor = legacy_descriptor(builtin, legacy_group);
            views.insert(descriptor.id, descriptor);
        }
        let next_window = legacy.next_window_id();
        let LegacySixWorkspace {
            main_layout,
            main_window,
            floating,
        } = legacy;
        let floating_windows = floating
            .into_iter()
            .map(|entry| {
                let id = entry.window_id;
                (
                    id,
                    FloatingWindowDescriptor {
                        id,
                        placement: entry.placement,
                        layout: DockLayout::Pane {
                            // Legacy main panes are allocated upward from one;
                            // temporary conversion panes allocate downward so
                            // they cannot collide with that deterministic tree.
                            pane_id: floating_pane_id(id),
                            items: vec![entry.view.id()],
                            active: 0,
                            extensions: BTreeMap::new(),
                        },
                        extensions: BTreeMap::new(),
                    },
                )
            })
            .collect();
        let document = Self {
            version: WORKSPACE_DOCUMENT_VERSION,
            allocators: WorkspaceIdAllocators {
                next_view: WorkspaceViewId::FIRST_DYNAMIC,
                next_window,
                next_link_group: 2,
            },
            views,
            main_layout: main_layout.into_v2(),
            main_window,
            floating_windows,
            link_groups: BTreeMap::from([(
                legacy_group,
                LinkGroupDescriptor {
                    label: Some("Legacy shared timeline".into()),
                    extensions: BTreeMap::new(),
                },
            )]),
            extensions: BTreeMap::new(),
            unknown_fields: BTreeMap::new(),
        };
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), WorkspaceDocumentError> {
        if self.version != WORKSPACE_DOCUMENT_VERSION {
            return Err(WorkspaceDocumentError::UnsupportedVersion(self.version));
        }
        if let Some(placement) = self.main_window {
            placement.validate()?;
        }
        for (id, descriptor) in &self.views {
            if *id != descriptor.id {
                return Err(WorkspaceDocumentError::ViewMapKeyMismatch(
                    *id,
                    descriptor.id,
                ));
            }
            descriptor.validate()?;
            if descriptor.links.group != LinkGroupId::UNLINKED
                && !self.link_groups.contains_key(&descriptor.links.group)
            {
                return Err(WorkspaceDocumentError::UnknownLinkGroup(
                    descriptor.links.group,
                ));
            }
        }
        if self.link_groups.contains_key(&LinkGroupId::UNLINKED) {
            return Err(WorkspaceDocumentError::ReservedLinkGroup);
        }
        for (id, window) in &self.floating_windows {
            if *id != window.id || id.0 == 0 {
                return Err(WorkspaceDocumentError::WindowMapKeyMismatch(*id, window.id));
            }
            if let Some(placement) = window.placement {
                placement.validate()?;
            }
        }

        let mut seen_views = BTreeSet::new();
        let mut seen_panes = BTreeSet::new();
        self.main_layout
            .validate_into(&self.views, &mut seen_views, &mut seen_panes)?;
        for window in self.floating_windows.values() {
            window
                .layout
                .validate_into(&self.views, &mut seen_views, &mut seen_panes)?;
        }
        for (view, descriptor) in &self.views {
            if descriptor.kind.close_behavior() == CloseBehavior::Pinned
                && !matches!(self.main_layout_location(*view), ViewLocation::Docked)
            {
                return Err(WorkspaceDocumentError::PinnedViewNotDocked(*view));
            }
        }
        self.validate_allocators()?;
        Ok(())
    }

    fn validate_allocators(&self) -> Result<(), WorkspaceDocumentError> {
        let maximum_view = self.views.keys().map(|id| id.0).max().unwrap_or(0);
        let maximum_window = self
            .floating_windows
            .keys()
            .map(|id| id.0)
            .max()
            .unwrap_or(0);
        let maximum_group = self.link_groups.keys().map(|id| id.0).max().unwrap_or(0);
        if self.allocators.next_view <= maximum_view
            || self.allocators.next_view < WorkspaceViewId::FIRST_DYNAMIC
            || self.allocators.next_window <= maximum_window
            || self.allocators.next_window == 0
            || self.allocators.next_link_group <= maximum_group
            || self.allocators.next_link_group == 0
        {
            return Err(WorkspaceDocumentError::AllocatorBehindExistingIdentity);
        }
        Ok(())
    }

    pub fn create_link_group(
        &mut self,
        descriptor: LinkGroupDescriptor,
    ) -> Result<LinkGroupId, WorkspaceDocumentError> {
        let id = LinkGroupId(self.allocators.next_link_group);
        self.allocators.next_link_group = self
            .allocators
            .next_link_group
            .checked_add(1)
            .ok_or(WorkspaceDocumentError::LinkGroupIdExhausted)?;
        self.link_groups.insert(id, descriptor);
        Ok(id)
    }

    pub fn create_view(
        &mut self,
        view: NewWorkspaceView,
    ) -> Result<WorkspaceViewId, WorkspaceDocumentError> {
        let id = WorkspaceViewId(self.allocators.next_view);
        self.allocators.next_view = self
            .allocators
            .next_view
            .checked_add(1)
            .ok_or(WorkspaceDocumentError::ViewIdExhausted)?;
        let descriptor = WorkspaceViewDescriptor {
            id,
            kind: view.kind,
            target: view.target,
            title_override: view.title_override,
            links: view.links,
            state: view.state,
            extensions: view.extensions,
        };
        descriptor.validate()?;
        if descriptor.links.group != LinkGroupId::UNLINKED
            && !self.link_groups.contains_key(&descriptor.links.group)
        {
            return Err(WorkspaceDocumentError::UnknownLinkGroup(
                descriptor.links.group,
            ));
        }
        self.views.insert(id, descriptor);
        Ok(id)
    }

    /// Replace a descriptor without changing its durable identity or its
    /// placement. Editor entities use this to publish target and view-local
    /// state changes back into the portable workspace document.
    pub fn replace_view(
        &mut self,
        descriptor: WorkspaceViewDescriptor,
    ) -> Result<(), WorkspaceDocumentError> {
        let id = descriptor.id;
        if !self.views.contains_key(&id) {
            return Err(WorkspaceDocumentError::UnknownView(id));
        }
        descriptor.validate()?;
        if descriptor.links.group != LinkGroupId::UNLINKED
            && !self.link_groups.contains_key(&descriptor.links.group)
        {
            return Err(WorkspaceDocumentError::UnknownLinkGroup(
                descriptor.links.group,
            ));
        }
        let mut next = self.clone();
        next.views.insert(id, descriptor);
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Replace the live main-window split/tab tree after translating a Guise
    /// snapshot back to durable view identities. The operation is atomic with
    /// respect to document validation.
    pub fn replace_main_layout(
        &mut self,
        layout: DockLayout,
    ) -> Result<(), WorkspaceDocumentError> {
        let mut next = self.clone();
        next.main_layout = layout;
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Replace one floating window's split/tab tree. A native window is a
    /// presentation of this descriptor; its GPUI handle is never persisted.
    pub fn replace_floating_layout(
        &mut self,
        window: WorkspaceWindowId,
        layout: DockLayout,
    ) -> Result<(), WorkspaceDocumentError> {
        let mut next = self.clone();
        next.floating_windows
            .get_mut(&window)
            .ok_or(WorkspaceDocumentError::UnknownWindow(window))?
            .layout = layout;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn set_main_window(
        &mut self,
        placement: Option<WindowPlacement>,
    ) -> Result<(), WorkspaceDocumentError> {
        if let Some(placement) = placement {
            placement.validate()?;
        }
        self.main_window = placement;
        Ok(())
    }

    pub fn set_floating_window_placement(
        &mut self,
        window: WorkspaceWindowId,
        placement: Option<WindowPlacement>,
    ) -> Result<(), WorkspaceDocumentError> {
        if let Some(placement) = placement {
            placement.validate()?;
        }
        self.floating_windows
            .get_mut(&window)
            .ok_or(WorkspaceDocumentError::UnknownWindow(window))?
            .placement = placement;
        Ok(())
    }

    pub fn location(&self, view: WorkspaceViewId) -> Result<ViewLocation, WorkspaceDocumentError> {
        if !self.views.contains_key(&view) {
            return Err(WorkspaceDocumentError::UnknownView(view));
        }
        Ok(self.main_layout_location(view))
    }

    /// Make a hidden descriptor visible in the main window. Already-visible
    /// descriptors are left in place, so commands may safely be replayed.
    pub fn show_view(&mut self, view: WorkspaceViewId) -> Result<(), WorkspaceDocumentError> {
        match self.location(view)? {
            ViewLocation::Hidden => {
                self.main_layout.add_to_primary(view);
                Ok(())
            }
            ViewLocation::Docked | ViewLocation::Floating(_) => Ok(()),
        }
    }

    /// Apply the descriptor's lifecycle policy. `Hide` retains the descriptor
    /// and editor state; `RemoveDescriptor` retires the instance identity.
    pub fn close_view(&mut self, view: WorkspaceViewId) -> Result<(), WorkspaceDocumentError> {
        let behavior = self
            .views
            .get(&view)
            .ok_or(WorkspaceDocumentError::UnknownView(view))?
            .kind
            .close_behavior();
        if behavior == CloseBehavior::Pinned {
            return Err(WorkspaceDocumentError::PinnedViewNotDocked(view));
        }

        let mut next = self.clone();
        next.remove_placement(view)?;
        if behavior == CloseBehavior::RemoveDescriptor {
            next.views.remove(&view);
        }
        next.validate()?;
        *self = next;
        Ok(())
    }

    fn main_layout_location(&self, view: WorkspaceViewId) -> ViewLocation {
        if layout_contains(&self.main_layout, view) {
            return ViewLocation::Docked;
        }
        self.floating_windows
            .iter()
            .find_map(|(window, descriptor)| {
                layout_contains(&descriptor.layout, view).then_some(ViewLocation::Floating(*window))
            })
            .unwrap_or(ViewLocation::Hidden)
    }

    /// Persist a floating single-view window. The app adapter is responsible for
    /// opening the native window *after* this durable operation succeeds.
    pub fn float_view(
        &mut self,
        view: WorkspaceViewId,
        placement: Option<WindowPlacement>,
    ) -> Result<WorkspaceWindowId, WorkspaceDocumentError> {
        let descriptor = self
            .views
            .get(&view)
            .ok_or(WorkspaceDocumentError::UnknownView(view))?;
        if !descriptor.kind.can_float() {
            return Err(WorkspaceDocumentError::ViewCannotFloat(view));
        }
        if let Some(placement) = placement {
            placement.validate()?;
        }
        if let ViewLocation::Floating(window) = self.location(view)? {
            return Ok(window);
        }
        let location = self.location(view)?;
        let main_layout = if location == ViewLocation::Docked {
            let (main_layout, removed) = self.main_layout.clone().without(view);
            debug_assert!(removed);
            Some(main_layout.ok_or(WorkspaceDocumentError::CannotEmptyMainWorkspace)?)
        } else {
            None
        };
        let window = WorkspaceWindowId(self.allocators.next_window);
        self.allocators.next_window = self
            .allocators
            .next_window
            .checked_add(1)
            .ok_or(WorkspaceDocumentError::WindowIdExhausted)?;
        if let Some(main_layout) = main_layout {
            self.main_layout = main_layout;
        }
        self.floating_windows.insert(
            window,
            FloatingWindowDescriptor {
                id: window,
                placement,
                layout: DockLayout::Pane {
                    pane_id: floating_pane_id(window),
                    items: vec![view],
                    active: 0,
                    extensions: BTreeMap::new(),
                },
                extensions: BTreeMap::new(),
            },
        );
        Ok(window)
    }

    /// Tear a view out of whichever tree currently owns it and place it in a
    /// fresh native window descriptor. Unlike [`float_view`](Self::float_view),
    /// this also supports splitting one item out of an existing floating
    /// multi-pane window.
    pub fn tear_off_view(
        &mut self,
        view: WorkspaceViewId,
        placement: Option<WindowPlacement>,
    ) -> Result<WorkspaceWindowId, WorkspaceDocumentError> {
        let descriptor = self
            .views
            .get(&view)
            .ok_or(WorkspaceDocumentError::UnknownView(view))?;
        if !descriptor.kind.can_float() {
            return Err(WorkspaceDocumentError::ViewCannotFloat(view));
        }
        if let Some(placement) = placement {
            placement.validate()?;
        }

        let mut next = self.clone();
        next.remove_placement(view)?;
        let window = WorkspaceWindowId(next.allocators.next_window);
        next.allocators.next_window = next
            .allocators
            .next_window
            .checked_add(1)
            .ok_or(WorkspaceDocumentError::WindowIdExhausted)?;
        next.floating_windows.insert(
            window,
            FloatingWindowDescriptor {
                id: window,
                placement,
                layout: DockLayout::Pane {
                    pane_id: floating_pane_id(window),
                    items: vec![view],
                    active: 0,
                    extensions: BTreeMap::new(),
                },
                extensions: BTreeMap::new(),
            },
        );
        next.validate()?;
        *self = next;
        Ok(window)
    }

    /// Dock one view rather than collapsing an entire floating window.
    pub fn dock_view(&mut self, view: WorkspaceViewId) -> Result<(), WorkspaceDocumentError> {
        match self.location(view)? {
            ViewLocation::Docked => return Ok(()),
            ViewLocation::Hidden => {
                self.main_layout.add_to_primary(view);
                return Ok(());
            }
            ViewLocation::Floating(_) => {}
        }
        let mut next = self.clone();
        next.remove_placement(view)?;
        next.main_layout.add_to_primary(view);
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Dock every view from a floating window into the primary main pane.
    pub fn dock_window(&mut self, window: WorkspaceWindowId) -> Result<(), WorkspaceDocumentError> {
        let floating = self
            .floating_windows
            .remove(&window)
            .ok_or(WorkspaceDocumentError::UnknownWindow(window))?;
        let mut items = Vec::new();
        collect_layout_items(&floating.layout, &mut items);
        for view in items {
            self.main_layout.add_to_primary(view);
        }
        Ok(())
    }

    fn remove_placement(&mut self, view: WorkspaceViewId) -> Result<(), WorkspaceDocumentError> {
        match self.location(view)? {
            ViewLocation::Hidden => Ok(()),
            ViewLocation::Docked => {
                let (layout, removed) = self.main_layout.clone().without(view);
                debug_assert!(removed);
                self.main_layout =
                    layout.ok_or(WorkspaceDocumentError::CannotEmptyMainWorkspace)?;
                Ok(())
            }
            ViewLocation::Floating(window) => {
                let floating = self
                    .floating_windows
                    .get(&window)
                    .ok_or(WorkspaceDocumentError::UnknownWindow(window))?;
                let (layout, removed) = floating.layout.clone().without(view);
                debug_assert!(removed);
                if let Some(layout) = layout {
                    self.floating_windows
                        .get_mut(&window)
                        .expect("floating window checked above")
                        .layout = layout;
                } else {
                    self.floating_windows.remove(&window);
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewLocation {
    Docked,
    Floating(WorkspaceWindowId),
    Hidden,
}

fn layout_contains(layout: &DockLayout, view: WorkspaceViewId) -> bool {
    match layout {
        DockLayout::Pane { items, .. } => items.contains(&view),
        DockLayout::Split { first, second, .. } => {
            layout_contains(first, view) || layout_contains(second, view)
        }
    }
}

fn collect_layout_items(layout: &DockLayout, out: &mut Vec<WorkspaceViewId>) {
    match layout {
        DockLayout::Pane { items, .. } => out.extend(items),
        DockLayout::Split { first, second, .. } => {
            collect_layout_items(first, out);
            collect_layout_items(second, out);
        }
    }
}

/// Main-pane IDs are allocated upward from one by the legacy migration. A
/// floating window gets a disjoint, deterministic document pane ID without
/// borrowing any runtime toolkit allocation scheme.
fn floating_pane_id(window: WorkspaceWindowId) -> DockPaneId {
    debug_assert_ne!(window.0, 0);
    DockPaneId(u64::MAX - (window.0 - 1))
}

/// Pure migration input for the existing conceptual six-view workspace. An
/// adapter at the old Guise boundary decodes its runtime snapshot into this
/// value; this document module never imports Guise to do so.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacySixWorkspace {
    pub main_layout: LegacySixDockLayout,
    pub main_window: Option<WindowPlacement>,
    pub floating: Vec<LegacyFloatingView>,
}

impl Default for LegacySixWorkspace {
    fn default() -> Self {
        Self {
            main_layout: LegacySixDockLayout::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.72,
                first: Box::new(LegacySixDockLayout::Pane {
                    items: vec![
                        LegacyBuiltinView::Track,
                        LegacyBuiltinView::Waterfall,
                        LegacyBuiltinView::Rhythm,
                        LegacyBuiltinView::Components,
                    ],
                    active: 0,
                }),
                second: Box::new(LegacySixDockLayout::Pane {
                    items: vec![LegacyBuiltinView::Loom, LegacyBuiltinView::Separation],
                    active: 0,
                }),
            },
            main_window: None,
            floating: Vec::new(),
        }
    }
}

impl LegacySixWorkspace {
    fn validate(&self) -> Result<(), WorkspaceDocumentError> {
        if let Some(placement) = self.main_window {
            placement.validate()?;
        }
        let mut views = BTreeSet::new();
        self.main_layout.validate_into(&mut views)?;
        if !views.contains(&LegacyBuiltinView::Track) {
            return Err(WorkspaceDocumentError::LegacyTrackMissing);
        }
        let mut windows = BTreeSet::new();
        for floating in &self.floating {
            if floating.window_id.0 == 0 || !windows.insert(floating.window_id) {
                return Err(WorkspaceDocumentError::DuplicateWindow(floating.window_id));
            }
            if floating.view == LegacyBuiltinView::Track {
                return Err(WorkspaceDocumentError::LegacyTrackFloating);
            }
            if !views.insert(floating.view) {
                return Err(WorkspaceDocumentError::DuplicatePlacement(
                    floating.view.id(),
                ));
            }
            if let Some(placement) = floating.placement {
                placement.validate()?;
            }
        }
        Ok(())
    }

    fn next_window_id(&self) -> u64 {
        self.floating
            .iter()
            .map(|floating| floating.window_id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LegacyFloatingView {
    pub window_id: WorkspaceWindowId,
    pub view: LegacyBuiltinView,
    pub placement: Option<WindowPlacement>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LegacySixDockLayout {
    Pane {
        items: Vec<LegacyBuiltinView>,
        active: usize,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<LegacySixDockLayout>,
        second: Box<LegacySixDockLayout>,
    },
}

impl LegacySixDockLayout {
    fn validate_into(
        &self,
        seen: &mut BTreeSet<LegacyBuiltinView>,
    ) -> Result<(), WorkspaceDocumentError> {
        match self {
            Self::Pane { items, active } => {
                if items.is_empty() || *active >= items.len() {
                    return Err(WorkspaceDocumentError::InvalidLayout(
                        "legacy pane must contain an active item",
                    ));
                }
                for view in items {
                    if !seen.insert(*view) {
                        return Err(WorkspaceDocumentError::DuplicatePlacement(view.id()));
                    }
                }
                Ok(())
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || *ratio <= 0.0 || *ratio >= 1.0 {
                    return Err(WorkspaceDocumentError::InvalidLayout(
                        "legacy split ratio must be finite and inside (0, 1)",
                    ));
                }
                first.validate_into(seen)?;
                second.validate_into(seen)
            }
        }
    }

    fn into_v2(self) -> DockLayout {
        fn convert(layout: LegacySixDockLayout, next_pane: &mut u64) -> DockLayout {
            match layout {
                LegacySixDockLayout::Pane { items, active } => {
                    let pane = DockPaneId(*next_pane);
                    *next_pane += 1;
                    DockLayout::Pane {
                        pane_id: pane,
                        items: items.into_iter().map(LegacyBuiltinView::id).collect(),
                        active,
                        extensions: BTreeMap::new(),
                    }
                }
                LegacySixDockLayout::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => DockLayout::Split {
                    axis,
                    ratio,
                    first: Box::new(convert(*first, next_pane)),
                    second: Box::new(convert(*second, next_pane)),
                    extensions: BTreeMap::new(),
                },
            }
        }
        let mut next_pane = 1;
        convert(self, &mut next_pane)
    }
}

fn legacy_descriptor(view: LegacyBuiltinView, group: LinkGroupId) -> WorkspaceViewDescriptor {
    let id = view.id();
    let (kind, target, state) = match view {
        LegacyBuiltinView::Track => (
            WorkspaceItemKind::Overview,
            EditorTarget::Project,
            EditorViewState::Overview {
                viewport: FrameViewport { start: 0, end: 1 },
                follow: false,
            },
        ),
        LegacyBuiltinView::Waterfall => legacy_analysis_descriptor(AnalysisLensKind::Waterfall),
        LegacyBuiltinView::Rhythm => legacy_analysis_descriptor(AnalysisLensKind::Rhythm),
        LegacyBuiltinView::Components => legacy_analysis_descriptor(AnalysisLensKind::Components),
        LegacyBuiltinView::Separation => legacy_analysis_descriptor(AnalysisLensKind::Separation),
        LegacyBuiltinView::Loom => legacy_analysis_descriptor(AnalysisLensKind::Loom),
    };
    WorkspaceViewDescriptor {
        id,
        kind,
        target,
        title_override: None,
        links: ViewLinkMembership {
            group,
            facets: LinkFacets::TIME,
        },
        state,
        extensions: BTreeMap::new(),
    }
}

fn legacy_analysis_descriptor(
    lens: AnalysisLensKind,
) -> (WorkspaceItemKind, EditorTarget, EditorViewState) {
    (
        WorkspaceItemKind::AnalysisLens { lens },
        EditorTarget::Analysis { source_id: None },
        EditorViewState::Analysis {
            viewport: FrameViewport { start: 0, end: 1 },
            follow: false,
            min_frequency_hz: None,
            max_frequency_hz: None,
            recipe_fingerprint: None,
        },
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceDocumentError {
    Json(String),
    UnsupportedVersion(u32),
    ZeroViewId,
    EmptyTitle(WorkspaceViewId),
    DescriptorMismatch(WorkspaceViewId),
    UnlinkedFacets(WorkspaceViewId),
    InvalidViewState(&'static str),
    UnknownView(WorkspaceViewId),
    DuplicatePlacement(WorkspaceViewId),
    DuplicatePane(DockPaneId),
    InvalidLayout(&'static str),
    InvalidWindowPlacement,
    WindowMapKeyMismatch(WorkspaceWindowId, WorkspaceWindowId),
    ViewMapKeyMismatch(WorkspaceViewId, WorkspaceViewId),
    DuplicateWindow(WorkspaceWindowId),
    UnknownWindow(WorkspaceWindowId),
    UnknownLinkGroup(LinkGroupId),
    ReservedLinkGroup,
    PinnedViewNotDocked(WorkspaceViewId),
    AllocatorBehindExistingIdentity,
    ViewIdExhausted,
    WindowIdExhausted,
    LinkGroupIdExhausted,
    ViewCannotFloat(WorkspaceViewId),
    ViewNotDocked(WorkspaceViewId),
    CannotEmptyMainWorkspace,
    LegacyTrackMissing,
    LegacyTrackFloating,
}

impl fmt::Display for WorkspaceDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(formatter, "workspace JSON: {message}"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported workspace document version {version}"
                )
            }
            Self::ZeroViewId => formatter.write_str("workspace view ID zero is reserved"),
            Self::EmptyTitle(view) => {
                write!(formatter, "workspace view {} has an empty title", view.0)
            }
            Self::DescriptorMismatch(view) => write!(
                formatter,
                "workspace view {} has incompatible kind, target, or state",
                view.0
            ),
            Self::UnlinkedFacets(view) => write!(
                formatter,
                "workspace view {} declares linked facets in the unlinked group",
                view.0
            ),
            Self::InvalidViewState(message) => formatter.write_str(message),
            Self::UnknownView(view) => write!(formatter, "workspace view {} is unknown", view.0),
            Self::DuplicatePlacement(view) => {
                write!(
                    formatter,
                    "workspace view {} is placed more than once",
                    view.0
                )
            }
            Self::DuplicatePane(pane) => {
                write!(formatter, "workspace pane {} is duplicate", pane.0)
            }
            Self::InvalidLayout(message) => formatter.write_str(message),
            Self::InvalidWindowPlacement => {
                formatter.write_str("invalid workspace window placement")
            }
            Self::WindowMapKeyMismatch(key, descriptor) => write!(
                formatter,
                "floating window map key {} differs from descriptor {}",
                key.0, descriptor.0
            ),
            Self::ViewMapKeyMismatch(key, descriptor) => write!(
                formatter,
                "view map key {} differs from descriptor {}",
                key.0, descriptor.0
            ),
            Self::DuplicateWindow(window) => {
                write!(formatter, "workspace window {} is duplicate", window.0)
            }
            Self::UnknownWindow(window) => {
                write!(formatter, "workspace window {} is unknown", window.0)
            }
            Self::UnknownLinkGroup(group) => write!(formatter, "link group {} is unknown", group.0),
            Self::ReservedLinkGroup => {
                formatter.write_str("link group zero is reserved for unlinked views")
            }
            Self::PinnedViewNotDocked(view) => write!(
                formatter,
                "pinned workspace view {} must remain docked in the main workspace",
                view.0
            ),
            Self::AllocatorBehindExistingIdentity => {
                formatter.write_str("workspace ID allocator would reuse an existing identity")
            }
            Self::ViewIdExhausted => formatter.write_str("workspace view IDs are exhausted"),
            Self::WindowIdExhausted => formatter.write_str("workspace window IDs are exhausted"),
            Self::LinkGroupIdExhausted => {
                formatter.write_str("workspace link group IDs are exhausted")
            }
            Self::ViewCannotFloat(view) => {
                write!(formatter, "workspace view {} cannot float", view.0)
            }
            Self::ViewNotDocked(view) => {
                write!(formatter, "workspace view {} is not docked", view.0)
            }
            Self::CannotEmptyMainWorkspace => {
                formatter.write_str("cannot empty the main workspace")
            }
            Self::LegacyTrackMissing => formatter.write_str("legacy workspace is missing Track"),
            Self::LegacyTrackFloating => formatter.write_str("legacy Track cannot float"),
        }
    }
}

impl Error for WorkspaceDocumentError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_six_migration_reserves_ids_and_allocates_dynamically() {
        let mut document = WorkspaceDocument::default();
        assert_eq!(document.views.len(), 6);
        assert_eq!(document.allocators.next_view, 7);
        let id = document
            .create_view(NewWorkspaceView {
                kind: WorkspaceItemKind::PatternEditor {
                    mode: PatternEditorMode::Steps,
                },
                target: EditorTarget::PatternDefinition { id: 42 },
                title_override: Some("Break".into()),
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
        assert_eq!(id, WorkspaceViewId(7));
        assert_eq!(document.allocators.next_view, 8);
        document.validate().unwrap();
    }

    #[test]
    fn snapshot_round_trips_unknown_top_level_and_local_extensions() {
        let source = r#"{
          "version": 2,
          "allocators": { "next_view": 7, "next_window": 1, "next_link_group": 2 },
          "views": {
            "1": {
              "id": 1, "kind": { "type": "overview" }, "target": { "type": "project" },
              "links": { "group": 1, "facets": 1 },
              "state": { "type": "overview", "viewport": { "start": 0, "end": 1 }, "follow": false },
              "extensions": { "future_view": { "color": "violet" } }
            }
          },
          "main_layout": { "type": "pane", "pane_id": 1, "items": [1], "active": 0 },
          "link_groups": { "1": { "label": "timeline" } },
          "future_top_level": { "retained": [1, 2, 3] }
        }"#;
        let document = WorkspaceDocument::from_json(source).unwrap();
        assert!(document.unknown_fields.contains_key("future_top_level"));
        let encoded: Value = serde_json::from_str(&document.to_json_pretty().unwrap()).unwrap();
        assert_eq!(
            encoded["future_top_level"]["retained"],
            Value::Array(vec![Value::from(1), Value::from(2), Value::from(3)])
        );
        assert_eq!(
            encoded["views"]["1"]["extensions"]["future_view"]["color"],
            Value::String("violet".into())
        );
    }

    #[test]
    fn float_and_dock_preserve_descriptor_identity_and_placement() {
        let mut document = WorkspaceDocument::default();
        let window = document
            .float_view(
                WorkspaceViewId::WATERFALL,
                Some(WindowPlacement {
                    mode: WindowMode::Windowed,
                    x: 20.0,
                    y: 30.0,
                    width: 640.0,
                    height: 480.0,
                }),
            )
            .unwrap();
        assert_eq!(
            document.location(WorkspaceViewId::WATERFALL).unwrap(),
            ViewLocation::Floating(window)
        );
        document.dock_window(window).unwrap();
        assert_eq!(
            document.location(WorkspaceViewId::WATERFALL).unwrap(),
            ViewLocation::Docked
        );
        document.validate().unwrap();
    }

    #[test]
    fn duplicate_placement_is_rejected_even_across_windows() {
        let mut document = WorkspaceDocument::default();
        document.floating_windows.insert(
            WorkspaceWindowId(1),
            FloatingWindowDescriptor {
                id: WorkspaceWindowId(1),
                placement: None,
                layout: DockLayout::Pane {
                    pane_id: DockPaneId(3),
                    items: vec![WorkspaceViewId::WATERFALL],
                    active: 0,
                    extensions: BTreeMap::new(),
                },
                extensions: BTreeMap::new(),
            },
        );
        assert_eq!(
            document.validate(),
            Err(WorkspaceDocumentError::DuplicatePlacement(
                WorkspaceViewId::WATERFALL
            ))
        );
    }

    #[test]
    fn close_policy_hides_editors_and_retires_analysis_instances() {
        let mut document = WorkspaceDocument::default();
        let pattern = document
            .create_view(NewWorkspaceView {
                kind: WorkspaceItemKind::PatternEditor {
                    mode: PatternEditorMode::Steps,
                },
                target: EditorTarget::PatternDefinition { id: 42 },
                title_override: None,
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
        document.show_view(pattern).unwrap();
        document.close_view(pattern).unwrap();
        assert!(document.views.contains_key(&pattern));
        assert_eq!(document.location(pattern).unwrap(), ViewLocation::Hidden);

        document.close_view(WorkspaceViewId::WATERFALL).unwrap();
        assert!(!document.views.contains_key(&WorkspaceViewId::WATERFALL));
        document.validate().unwrap();
    }

    #[test]
    fn tear_off_and_dock_one_view_preserve_instance_identity() {
        let mut document = WorkspaceDocument::default();
        let view = WorkspaceViewId::RHYTHM;
        let window = document.tear_off_view(view, None).unwrap();
        assert_eq!(
            document.location(view).unwrap(),
            ViewLocation::Floating(window)
        );
        document.dock_view(view).unwrap();
        assert_eq!(document.location(view).unwrap(), ViewLocation::Docked);
        assert!(document.views.contains_key(&view));
        document.validate().unwrap();
    }
}
