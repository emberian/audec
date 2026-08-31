//! Persistent dock/tab workspace state and the thin Guise `PaneGroup` bridge.
//!
//! This module deliberately knows nothing about `Workbench` or `Visualizer`.
//! The application owns those entities and supplies render/title callbacks;
//! this module owns stable view identity, layout validation and persistence,
//! and the float/dock-back state machine.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::error::Error;
use std::fmt;

use gpui::{
    point, px, size, AnyElement, App, AppContext as _, Bounds, Context, Entity, Hsla, Pixels,
    SharedString, Window, WindowBounds,
};
use guise::panegroup::{ItemId, ItemIds, LayoutSnapshot};
use guise::{PaneGroup, SplitDirection};
use serde::{Deserialize, Serialize};

use crate::workspace_document::{
    DockLayout as DocumentDockLayout, DockPaneId, LegacyBuiltinView, LegacyFloatingView,
    LegacySixDockLayout, LegacySixWorkspace, NewWorkspaceView, SplitAxis as DocumentSplitAxis,
    WindowMode as DocumentWindowMode, WindowPlacement as DocumentWindowPlacement,
    WorkspaceDocument, WorkspaceDocumentError, WorkspaceViewDescriptor,
    WorkspaceViewId as DocumentViewId, WorkspaceWindowId as DocumentWindowId,
};

pub const WORKSPACE_SNAPSHOT_VERSION: u32 = 1;

/// Stable, persisted identity for a workspace view.
///
/// These numbers are also the raw item numbers in Guise's encoded
/// `LayoutSnapshot`. Never renumber a shipped view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceViewId(pub u64);

/// The built-in views available in the first dockable audec workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuiltinView {
    Track,
    Waterfall,
    Rhythm,
    Components,
    Separation,
    Loom,
}

impl BuiltinView {
    pub const ALL: [Self; 6] = [
        Self::Track,
        Self::Waterfall,
        Self::Rhythm,
        Self::Components,
        Self::Separation,
        Self::Loom,
    ];

    pub const fn id(self) -> WorkspaceViewId {
        WorkspaceViewId(match self {
            Self::Track => 1,
            Self::Waterfall => 2,
            Self::Rhythm => 3,
            Self::Components => 4,
            Self::Separation => 5,
            Self::Loom => 6,
        })
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Track => "Track",
            Self::Waterfall => "Spectral waterfall",
            Self::Rhythm => "Rhythm",
            Self::Components => "Components",
            Self::Separation => "Separation",
            Self::Loom => "Loom",
        }
    }

    pub const fn from_id(id: WorkspaceViewId) -> Option<Self> {
        match id.0 {
            1 => Some(Self::Track),
            2 => Some(Self::Waterfall),
            3 => Some(Self::Rhythm),
            4 => Some(Self::Components),
            5 => Some(Self::Separation),
            6 => Some(Self::Loom),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        self.id().0 as usize - 1
    }
}

/// Deterministically allocated Guise IDs for the built-in view roster.
///
/// `ItemId`'s raw constructor is private in Guise 1.5.3. Allocating in the
/// fixed `BuiltinView::ALL` order makes the runtime IDs equal the stable IDs
/// embedded in `LayoutSnapshot` strings.
#[derive(Clone, Debug)]
pub struct BuiltinItemIds {
    ids: Vec<ItemId>,
}

impl Default for BuiltinItemIds {
    fn default() -> Self {
        Self::new()
    }
}

impl BuiltinItemIds {
    pub fn new() -> Self {
        let mut allocator = ItemIds::new();
        let ids = BuiltinView::ALL.iter().map(|_| allocator.next()).collect();
        Self { ids }
    }

    pub fn item(&self, view: BuiltinView) -> ItemId {
        self.ids[view.index()]
    }

    pub fn view(&self, item: ItemId) -> Option<BuiltinView> {
        BuiltinView::ALL
            .into_iter()
            .find(|view| self.item(*view) == item)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

impl From<SplitAxis> for SplitDirection {
    fn from(axis: SplitAxis) -> Self {
        match axis {
            SplitAxis::Horizontal => Self::Horizontal,
            SplitAxis::Vertical => Self::Vertical,
        }
    }
}

impl From<SplitDirection> for SplitAxis {
    fn from(axis: SplitDirection) -> Self {
        match axis {
            SplitDirection::Horizontal => Self::Horizontal,
            SplitDirection::Vertical => Self::Vertical,
        }
    }
}

/// Typed, host-facing form of Guise's split/tab tree.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceLayout {
    Pane {
        items: Vec<BuiltinView>,
        active: usize,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<WorkspaceLayout>,
        second: Box<WorkspaceLayout>,
    },
}

impl WorkspaceLayout {
    pub fn default_edit() -> Self {
        Self::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.72,
            first: Box::new(Self::Pane {
                items: vec![
                    BuiltinView::Track,
                    BuiltinView::Waterfall,
                    BuiltinView::Rhythm,
                    BuiltinView::Components,
                ],
                active: 0,
            }),
            second: Box::new(Self::Pane {
                items: vec![BuiltinView::Loom, BuiltinView::Separation],
                active: 0,
            }),
        }
    }

    pub fn to_guise(&self) -> LayoutSnapshot {
        match self {
            Self::Pane { items, active } => LayoutSnapshot::Pane {
                items: items.iter().map(|view| view.id().0).collect(),
                active: *active,
            },
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutSnapshot::Split {
                axis: (*axis).into(),
                ratio: *ratio,
                first: Box::new(first.to_guise()),
                second: Box::new(second.to_guise()),
            },
        }
    }

    pub fn from_guise(snapshot: &LayoutSnapshot) -> Result<Self, WorkspaceError> {
        let layout = match snapshot {
            LayoutSnapshot::Pane { items, active } => {
                let mut views = Vec::with_capacity(items.len());
                for raw in items {
                    let id = WorkspaceViewId(*raw);
                    views.push(BuiltinView::from_id(id).ok_or(WorkspaceError::UnknownView(id))?);
                }
                Self::Pane {
                    items: views,
                    active: *active,
                }
            }
            LayoutSnapshot::Split {
                axis,
                ratio,
                first,
                second,
            } => Self::Split {
                axis: (*axis).into(),
                ratio: *ratio,
                first: Box::new(Self::from_guise(first)?),
                second: Box::new(Self::from_guise(second)?),
            },
        };
        layout.validate()?;
        Ok(layout)
    }

    pub fn validate(&self) -> Result<(), WorkspaceError> {
        let mut seen = HashSet::new();
        self.validate_into(&mut seen)
    }

    fn validate_into(&self, seen: &mut HashSet<BuiltinView>) -> Result<(), WorkspaceError> {
        match self {
            Self::Pane { items, active } => {
                if items.is_empty() {
                    return Err(WorkspaceError::InvalidLayout("a pane is empty"));
                }
                if *active >= items.len() {
                    return Err(WorkspaceError::InvalidLayout(
                        "a pane active index is out of range",
                    ));
                }
                for view in items {
                    if !seen.insert(*view) {
                        return Err(WorkspaceError::DuplicateView(view.id()));
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
                    return Err(WorkspaceError::InvalidLayout(
                        "a split ratio is not finite and inside (0, 1)",
                    ));
                }
                first.validate_into(seen)?;
                second.validate_into(seen)
            }
        }
    }

    pub fn contains(&self, view: BuiltinView) -> bool {
        match self {
            Self::Pane { items, .. } => items.contains(&view),
            Self::Split { first, second, .. } => first.contains(view) || second.contains(view),
        }
    }

    pub fn activate(&mut self, view: BuiltinView) -> bool {
        match self {
            Self::Pane { items, active } => match items.iter().position(|item| *item == view) {
                Some(index) => {
                    *active = index;
                    true
                }
                None => false,
            },
            Self::Split { first, second, .. } => first.activate(view) || second.activate(view),
        }
    }

    pub fn remove(&mut self, view: BuiltinView) -> bool {
        let original = self.clone();
        let (next, removed) = original.without(view);
        if let Some(next) = next {
            *self = next;
            removed
        } else {
            false
        }
    }

    fn without(self, view: BuiltinView) -> (Option<Self>, bool) {
        match self {
            Self::Pane {
                mut items,
                mut active,
            } => {
                let Some(index) = items.iter().position(|item| *item == view) else {
                    return (Some(Self::Pane { items, active }), false);
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
                (Some(Self::Pane { items, active }), true)
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (first, first_removed) = first.without(view);
                let (second, second_removed) = second.without(view);
                let removed = first_removed || second_removed;
                let next = match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                    (None, None) => None,
                };
                (next, removed)
            }
        }
    }

    /// Add to the first (top-left) pane and make the item active.
    pub fn add_to_primary(&mut self, view: BuiltinView) -> bool {
        if self.contains(view) {
            return self.activate(view);
        }
        match self {
            Self::Pane { items, active } => {
                items.push(view);
                *active = items.len() - 1;
            }
            Self::Split { first, .. } => {
                first.add_to_primary(view);
            }
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FloatingWindowId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowModeDto {
    Windowed,
    Maximized,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacementDto {
    pub mode: WindowModeDto,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowPlacementDto {
    pub fn validate(self) -> Result<(), WorkspaceError> {
        let values = [self.x, self.y, self.width, self.height];
        if !values.into_iter().all(f32::is_finite) {
            return Err(WorkspaceError::InvalidWindowPlacement);
        }
        if self.width <= 0.0 || self.height <= 0.0 {
            return Err(WorkspaceError::InvalidWindowPlacement);
        }
        Ok(())
    }

    pub fn from_gpui(bounds: WindowBounds) -> Self {
        let mode = match bounds {
            WindowBounds::Windowed(_) => WindowModeDto::Windowed,
            WindowBounds::Maximized(_) => WindowModeDto::Maximized,
            WindowBounds::Fullscreen(_) => WindowModeDto::Fullscreen,
        };
        let bounds = bounds.get_bounds();
        Self {
            mode,
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }

    pub fn to_gpui(self) -> Result<WindowBounds, WorkspaceError> {
        self.validate()?;
        let bounds: Bounds<Pixels> = Bounds::new(
            point(px(self.x), px(self.y)),
            size(px(self.width), px(self.height)),
        );
        Ok(match self.mode {
            WindowModeDto::Windowed => WindowBounds::Windowed(bounds),
            WindowModeDto::Maximized => WindowBounds::Maximized(bounds),
            WindowModeDto::Fullscreen => WindowBounds::Fullscreen(bounds),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FloatingViewDto {
    pub window_id: FloatingWindowId,
    pub view_id: WorkspaceViewId,
    pub placement: Option<WindowPlacementDto>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshotDto {
    pub version: u32,
    /// Guise's compact split/tab snapshot, containing stable numeric view IDs.
    pub main_layout: String,
    pub main_window: Option<WindowPlacementDto>,
    pub floating: Vec<FloatingViewDto>,
}

impl WorkspaceSnapshotDto {
    pub fn to_json_pretty(&self) -> Result<String, WorkspaceError> {
        serde_json::to_string_pretty(self).map_err(|error| WorkspaceError::Json(error.to_string()))
    }

    pub fn from_json(source: &str) -> Result<Self, WorkspaceError> {
        let snapshot: Self = serde_json::from_str(source)
            .map_err(|error| WorkspaceError::Json(error.to_string()))?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), WorkspaceError> {
        if self.version != WORKSPACE_SNAPSHOT_VERSION {
            return Err(WorkspaceError::UnsupportedVersion(self.version));
        }
        if let Some(placement) = self.main_window {
            placement.validate()?;
        }

        let guise = LayoutSnapshot::decode(&self.main_layout)
            .map_err(|error| WorkspaceError::Snapshot(error.to_string()))?;
        let layout = WorkspaceLayout::from_guise(&guise)?;
        if !layout.contains(BuiltinView::Track) {
            return Err(WorkspaceError::PinnedTrackMissing);
        }

        let mut views = HashSet::new();
        let mut windows = HashSet::new();
        collect_layout_views(&layout, &mut views);
        for floating in &self.floating {
            let view = BuiltinView::from_id(floating.view_id)
                .ok_or(WorkspaceError::UnknownView(floating.view_id))?;
            if view == BuiltinView::Track {
                return Err(WorkspaceError::PinnedView(floating.view_id));
            }
            if !views.insert(view) {
                return Err(WorkspaceError::DuplicateView(floating.view_id));
            }
            if floating.window_id.0 == 0 || !windows.insert(floating.window_id) {
                return Err(WorkspaceError::DuplicateWindow(floating.window_id));
            }
            if let Some(placement) = floating.placement {
                placement.validate()?;
            }
        }
        Ok(())
    }
}

fn collect_layout_views(layout: &WorkspaceLayout, out: &mut HashSet<BuiltinView>) {
    match layout {
        WorkspaceLayout::Pane { items, .. } => out.extend(items),
        WorkspaceLayout::Split { first, second, .. } => {
            collect_layout_views(first, out);
            collect_layout_views(second, out);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewLocation {
    Docked,
    Floating(FloatingWindowId),
    Hidden,
}

/// Pure controller state. Native `AnyWindowHandle`s stay in the UI host; this
/// model persists only stable window keys and placements.
#[derive(Clone, Debug)]
pub struct WorkspaceModel {
    item_ids: BuiltinItemIds,
    main_layout: WorkspaceLayout,
    main_window: Option<WindowPlacementDto>,
    floating: BTreeMap<FloatingWindowId, FloatingViewDto>,
    next_window_id: u64,
}

impl Default for WorkspaceModel {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceModel {
    pub fn new() -> Self {
        Self {
            item_ids: BuiltinItemIds::new(),
            main_layout: WorkspaceLayout::default_edit(),
            main_window: None,
            floating: BTreeMap::new(),
            next_window_id: 1,
        }
    }

    pub fn from_snapshot(snapshot: WorkspaceSnapshotDto) -> Result<Self, WorkspaceError> {
        snapshot.validate()?;
        let guise = LayoutSnapshot::decode(&snapshot.main_layout)
            .map_err(|error| WorkspaceError::Snapshot(error.to_string()))?;
        let main_layout = WorkspaceLayout::from_guise(&guise)?;
        let floating = snapshot
            .floating
            .into_iter()
            .map(|entry| (entry.window_id, entry))
            .collect::<BTreeMap<_, _>>();
        let next_window_id = floating
            .keys()
            .map(|key| key.0)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(WorkspaceError::WindowIdExhausted)?;
        Ok(Self {
            item_ids: BuiltinItemIds::new(),
            main_layout,
            main_window: snapshot.main_window,
            floating,
            next_window_id,
        })
    }

    pub fn snapshot(&self) -> WorkspaceSnapshotDto {
        WorkspaceSnapshotDto {
            version: WORKSPACE_SNAPSHOT_VERSION,
            main_layout: self.main_layout.to_guise().encode(),
            main_window: self.main_window,
            floating: self.floating.values().cloned().collect(),
        }
    }

    pub fn item_ids(&self) -> &BuiltinItemIds {
        &self.item_ids
    }

    pub fn item(&self, view: BuiltinView) -> ItemId {
        self.item_ids.item(view)
    }

    pub fn view(&self, item: ItemId) -> Option<BuiltinView> {
        self.item_ids.view(item)
    }

    pub fn main_layout(&self) -> &WorkspaceLayout {
        &self.main_layout
    }

    pub fn guise_layout(&self) -> LayoutSnapshot {
        self.main_layout.to_guise()
    }

    pub fn main_window(&self) -> Option<WindowPlacementDto> {
        self.main_window
    }

    pub fn set_main_window(
        &mut self,
        placement: Option<WindowPlacementDto>,
    ) -> Result<(), WorkspaceError> {
        if let Some(placement) = placement {
            placement.validate()?;
        }
        self.main_window = placement;
        Ok(())
    }

    pub fn floating(&self) -> impl Iterator<Item = &FloatingViewDto> {
        self.floating.values()
    }

    pub fn location(&self, view: BuiltinView) -> ViewLocation {
        if self.main_layout.contains(view) {
            return ViewLocation::Docked;
        }
        self.floating
            .values()
            .find(|entry| entry.view_id == view.id())
            .map_or(ViewLocation::Hidden, |entry| {
                ViewLocation::Floating(entry.window_id)
            })
    }

    pub fn activate(&mut self, view: BuiltinView) -> bool {
        self.main_layout.activate(view)
    }

    /// Record a view moving to a native window. This accepts a hidden view as
    /// well as a docked one, which makes it safe after Guise has already
    /// detached the tab and an observer has synchronized the main snapshot.
    pub fn float_view(
        &mut self,
        view: BuiltinView,
        placement: Option<WindowPlacementDto>,
    ) -> Result<FloatingWindowId, WorkspaceError> {
        if view == BuiltinView::Track {
            return Err(WorkspaceError::PinnedView(view.id()));
        }
        if let Some(placement) = placement {
            placement.validate()?;
        }
        if let ViewLocation::Floating(window) = self.location(view) {
            return Ok(window);
        }

        self.main_layout.remove(view);
        let window_id = FloatingWindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .ok_or(WorkspaceError::WindowIdExhausted)?;
        self.floating.insert(
            window_id,
            FloatingViewDto {
                window_id,
                view_id: view.id(),
                placement,
            },
        );
        Ok(window_id)
    }

    /// Dock a floating item into the primary pane. Returns `false` when it was
    /// already docked, making a Dock button and native close hook idempotent.
    pub fn dock_back(&mut self, view: BuiltinView) -> Result<bool, WorkspaceError> {
        if self.main_layout.contains(view) {
            return Ok(false);
        }
        let window = self
            .floating
            .iter()
            .find_map(|(window, entry)| (entry.view_id == view.id()).then_some(*window));
        let Some(window) = window else {
            return Err(WorkspaceError::ViewNotFloating(view.id()));
        };
        self.floating.remove(&window);
        self.main_layout.add_to_primary(view);
        Ok(true)
    }

    pub fn update_floating_placement(
        &mut self,
        window: FloatingWindowId,
        placement: WindowPlacementDto,
    ) -> Result<(), WorkspaceError> {
        placement.validate()?;
        let entry = self
            .floating
            .get_mut(&window)
            .ok_or(WorkspaceError::UnknownWindow(window))?;
        entry.placement = Some(placement);
        Ok(())
    }

    /// Synchronize from the live Guise group after a drag, split, reorder, or
    /// divider resize. Floating views are rejected if they also appear in the
    /// main group.
    pub fn replace_main_layout(&mut self, snapshot: &LayoutSnapshot) -> Result<(), WorkspaceError> {
        let layout = WorkspaceLayout::from_guise(snapshot)?;
        if !layout.contains(BuiltinView::Track) {
            return Err(WorkspaceError::PinnedTrackMissing);
        }
        for entry in self.floating.values() {
            let view = BuiltinView::from_id(entry.view_id)
                .ok_or(WorkspaceError::UnknownView(entry.view_id))?;
            if layout.contains(view) {
                return Err(WorkspaceError::DuplicateView(entry.view_id));
            }
        }
        self.main_layout = layout;
        Ok(())
    }
}

/// Construct and restore the Guise entity without referring to audec's
/// concrete view types. The host callbacks map opaque `ItemId`s to entities.
pub fn create_pane_group<Host>(
    model: &WorkspaceModel,
    render_item: impl Fn(ItemId, &mut Window, &mut App) -> AnyElement + 'static,
    item_title: impl Fn(ItemId, &App) -> SharedString + 'static,
    cx: &mut Context<Host>,
) -> Entity<PaneGroup>
where
    Host: 'static,
{
    let first = model.item(BuiltinView::Track);
    let layout = model.guise_layout();
    let group = cx.new(|cx| {
        PaneGroup::new(first, cx)
            .tab_height(30.0)
            .on_render_item(render_item)
            .on_item_title(item_title)
    });
    group.update(cx, |group, cx| {
        let restored = group.restore(&layout, cx);
        debug_assert!(restored, "validated default workspace must restore");
    });
    group
}

/// Same bridge with optional per-tab status dots.
pub fn create_pane_group_with_dots<Host>(
    model: &WorkspaceModel,
    render_item: impl Fn(ItemId, &mut Window, &mut App) -> AnyElement + 'static,
    item_title: impl Fn(ItemId, &App) -> SharedString + 'static,
    item_dot: impl Fn(ItemId, &App) -> Option<Hsla> + 'static,
    cx: &mut Context<Host>,
) -> Entity<PaneGroup>
where
    Host: 'static,
{
    let first = model.item(BuiltinView::Track);
    let layout = model.guise_layout();
    let group = cx.new(|cx| {
        PaneGroup::new(first, cx)
            .tab_height(30.0)
            .on_render_item(render_item)
            .on_item_title(item_title)
            .on_item_dot(item_dot)
    });
    group.update(cx, |group, cx| {
        let restored = group.restore(&layout, cx);
        debug_assert!(restored, "validated default workspace must restore");
    });
    group
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceError {
    Snapshot(String),
    Json(String),
    UnknownView(WorkspaceViewId),
    DuplicateView(WorkspaceViewId),
    PinnedView(WorkspaceViewId),
    PinnedTrackMissing,
    InvalidLayout(&'static str),
    InvalidWindowPlacement,
    DuplicateWindow(FloatingWindowId),
    UnknownWindow(FloatingWindowId),
    ViewNotFloating(WorkspaceViewId),
    UnsupportedVersion(u32),
    WindowIdExhausted,
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(message) => write!(formatter, "workspace layout: {message}"),
            Self::Json(message) => write!(formatter, "workspace JSON: {message}"),
            Self::UnknownView(id) => write!(formatter, "unknown workspace view {}", id.0),
            Self::DuplicateView(id) => write!(formatter, "workspace view {} appears twice", id.0),
            Self::PinnedView(id) => write!(formatter, "workspace view {} is pinned", id.0),
            Self::PinnedTrackMissing => formatter.write_str("the pinned track view is missing"),
            Self::InvalidLayout(message) => {
                write!(formatter, "invalid workspace layout: {message}")
            }
            Self::InvalidWindowPlacement => {
                formatter.write_str("invalid workspace window placement")
            }
            Self::DuplicateWindow(id) => {
                write!(formatter, "floating window {} appears twice", id.0)
            }
            Self::UnknownWindow(id) => write!(formatter, "unknown floating window {}", id.0),
            Self::ViewNotFloating(id) => {
                write!(formatter, "workspace view {} is not floating", id.0)
            }
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported workspace snapshot version {version}"
                )
            }
            Self::WindowIdExhausted => formatter.write_str("floating window IDs are exhausted"),
        }
    }
}

impl Error for WorkspaceError {}

// -------------------------------------------------------------------------
// Dynamic v2 workspace adapter

/// Runtime-only Guise identity table. Persisted [`DocumentViewId`] values are
/// deliberately not smuggled into Guise's private `ItemId` representation.
/// The table may therefore be rebuilt on every launch without perturbing the
/// durable workspace document.
#[derive(Clone, Debug)]
pub struct RuntimeItemMap {
    allocator: ItemIds,
    next_raw: u64,
    by_view: BTreeMap<DocumentViewId, RuntimeItem>,
    by_raw: BTreeMap<u64, DocumentViewId>,
}

#[derive(Clone, Copy, Debug)]
struct RuntimeItem {
    item: ItemId,
    raw: u64,
}

impl RuntimeItemMap {
    pub fn from_document(document: &WorkspaceDocument) -> Self {
        let mut map = Self {
            allocator: ItemIds::new(),
            next_raw: 1,
            by_view: BTreeMap::new(),
            by_raw: BTreeMap::new(),
        };
        for view in document.views.keys().copied() {
            map.ensure(view);
        }
        map
    }

    /// Allocate an ephemeral Guise item for a newly created descriptor.
    pub fn ensure(&mut self, view: DocumentViewId) -> ItemId {
        if let Some(runtime) = self.by_view.get(&view) {
            return runtime.item;
        }
        let item = self.allocator.next();
        let raw = self.next_raw;
        self.next_raw = self.next_raw.saturating_add(1);
        self.by_view.insert(view, RuntimeItem { item, raw });
        self.by_raw.insert(raw, view);
        item
    }

    pub fn item(&self, view: DocumentViewId) -> Option<ItemId> {
        self.by_view.get(&view).map(|runtime| runtime.item)
    }

    pub fn view(&self, item: ItemId) -> Option<DocumentViewId> {
        self.by_view
            .iter()
            .find_map(|(view, runtime)| (runtime.item == item).then_some(*view))
    }

    fn raw(&self, view: DocumentViewId) -> Option<u64> {
        self.by_view.get(&view).map(|runtime| runtime.raw)
    }

    fn view_from_raw(&self, raw: u64) -> Option<DocumentViewId> {
        self.by_raw.get(&raw).copied()
    }

    pub fn forget(&mut self, view: DocumentViewId) -> bool {
        let Some(runtime) = self.by_view.remove(&view) else {
            return false;
        };
        self.by_raw.remove(&runtime.raw);
        true
    }

    pub fn bindings(&self) -> impl ExactSizeIterator<Item = (DocumentViewId, ItemId)> + '_ {
        self.by_view
            .iter()
            .map(|(view, runtime)| (*view, runtime.item))
    }

    pub fn runtime_bindings(
        &self,
    ) -> impl ExactSizeIterator<Item = (u64, DocumentViewId, ItemId)> + '_ {
        self.by_view
            .iter()
            .map(|(view, runtime)| (runtime.raw, *view, runtime.item))
    }
}

/// A v2 workspace document paired with its process-local Guise identities.
/// Musical/project truth remains outside this type; it owns presentation
/// descriptors and split/tab/native-window placement only.
#[derive(Clone, Debug)]
pub struct DynamicWorkspaceModel {
    document: WorkspaceDocument,
    items: RuntimeItemMap,
}

impl DynamicWorkspaceModel {
    pub fn new(document: WorkspaceDocument) -> Result<Self, DynamicWorkspaceError> {
        document.validate()?;
        let items = RuntimeItemMap::from_document(&document);
        Ok(Self { document, items })
    }

    pub fn from_legacy_snapshot(
        snapshot: WorkspaceSnapshotDto,
    ) -> Result<Self, DynamicWorkspaceError> {
        let document = migrate_legacy_snapshot(snapshot)?;
        Self::new(document)
    }

    pub fn document(&self) -> &WorkspaceDocument {
        &self.document
    }

    pub fn export_document(&self) -> WorkspaceDocument {
        self.document.clone()
    }

    pub fn item_map(&self) -> &RuntimeItemMap {
        &self.items
    }

    pub fn descriptor(&self, view: DocumentViewId) -> Option<&WorkspaceViewDescriptor> {
        self.document.views.get(&view)
    }

    pub fn item(&self, view: DocumentViewId) -> Option<ItemId> {
        self.items.item(view)
    }

    pub fn view(&self, item: ItemId) -> Option<DocumentViewId> {
        self.items.view(item)
    }

    pub fn main_guise_layout(&self) -> Result<LayoutSnapshot, DynamicWorkspaceError> {
        layout_to_guise(&self.document.main_layout, &self.items)
    }

    pub fn floating_guise_layout(
        &self,
        window: DocumentWindowId,
    ) -> Result<LayoutSnapshot, DynamicWorkspaceError> {
        let floating = self
            .document
            .floating_windows
            .get(&window)
            .ok_or(DynamicWorkspaceError::UnknownWindow(window))?;
        layout_to_guise(&floating.layout, &self.items)
    }

    pub fn create_view(
        &mut self,
        descriptor: NewWorkspaceView,
    ) -> Result<(DocumentViewId, ItemId), DynamicWorkspaceError> {
        let view = self.document.create_view(descriptor)?;
        let item = self.items.ensure(view);
        Ok((view, item))
    }

    pub fn replace_view(
        &mut self,
        descriptor: WorkspaceViewDescriptor,
    ) -> Result<(), DynamicWorkspaceError> {
        self.document.replace_view(descriptor)?;
        Ok(())
    }

    pub fn show_view(&mut self, view: DocumentViewId) -> Result<(), DynamicWorkspaceError> {
        self.document.show_view(view)?;
        Ok(())
    }

    pub fn close_view(&mut self, view: DocumentViewId) -> Result<(), DynamicWorkspaceError> {
        self.document.close_view(view)?;
        if !self.document.views.contains_key(&view) {
            self.items.forget(view);
        }
        Ok(())
    }

    pub fn float_view(
        &mut self,
        view: DocumentViewId,
        placement: Option<DocumentWindowPlacement>,
    ) -> Result<DocumentWindowId, DynamicWorkspaceError> {
        Ok(self.document.float_view(view, placement)?)
    }

    pub fn tear_off_view(
        &mut self,
        view: DocumentViewId,
        placement: Option<DocumentWindowPlacement>,
    ) -> Result<DocumentWindowId, DynamicWorkspaceError> {
        Ok(self.document.tear_off_view(view, placement)?)
    }

    pub fn dock_view(&mut self, view: DocumentViewId) -> Result<(), DynamicWorkspaceError> {
        self.document.dock_view(view)?;
        Ok(())
    }

    pub fn dock_window(&mut self, window: DocumentWindowId) -> Result<(), DynamicWorkspaceError> {
        self.document.dock_window(window)?;
        Ok(())
    }

    pub fn replace_main_layout(
        &mut self,
        snapshot: &LayoutSnapshot,
    ) -> Result<(), DynamicWorkspaceError> {
        let layout = self.translate_from_guise(snapshot, Some(&self.document.main_layout))?;
        self.document.replace_main_layout(layout)?;
        Ok(())
    }

    pub fn replace_floating_layout(
        &mut self,
        window: DocumentWindowId,
        snapshot: &LayoutSnapshot,
    ) -> Result<(), DynamicWorkspaceError> {
        let previous = self
            .document
            .floating_windows
            .get(&window)
            .ok_or(DynamicWorkspaceError::UnknownWindow(window))?
            .layout
            .clone();
        let layout = self.translate_from_guise(snapshot, Some(&previous))?;
        self.document.replace_floating_layout(window, layout)?;
        Ok(())
    }

    pub fn set_main_window(
        &mut self,
        placement: Option<DocumentWindowPlacement>,
    ) -> Result<(), DynamicWorkspaceError> {
        self.document.set_main_window(placement)?;
        Ok(())
    }

    pub fn set_floating_window_placement(
        &mut self,
        window: DocumentWindowId,
        placement: Option<DocumentWindowPlacement>,
    ) -> Result<(), DynamicWorkspaceError> {
        self.document
            .set_floating_window_placement(window, placement)?;
        Ok(())
    }

    fn translate_from_guise(
        &self,
        snapshot: &LayoutSnapshot,
        previous: Option<&DocumentDockLayout>,
    ) -> Result<DocumentDockLayout, DynamicWorkspaceError> {
        let mut occupied = BTreeSet::new();
        collect_document_panes(&self.document.main_layout, &mut occupied);
        for floating in self.document.floating_windows.values() {
            collect_document_panes(&floating.layout, &mut occupied);
        }

        let mut previous_panes = VecDeque::new();
        let mut previous_splits = VecDeque::new();
        if let Some(previous) = previous {
            collect_layout_metadata(previous, &mut previous_panes, &mut previous_splits);
            for (pane, _) in &previous_panes {
                occupied.remove(pane);
            }
        }
        let mut next_pane = 1_u64;
        guise_to_layout(
            snapshot,
            &self.items,
            &mut occupied,
            &mut next_pane,
            &mut previous_panes,
            &mut previous_splits,
        )
    }
}

fn layout_to_guise(
    layout: &DocumentDockLayout,
    items: &RuntimeItemMap,
) -> Result<LayoutSnapshot, DynamicWorkspaceError> {
    Ok(match layout {
        DocumentDockLayout::Pane {
            items: views,
            active,
            ..
        } => LayoutSnapshot::Pane {
            items: views
                .iter()
                .map(|view| {
                    items
                        .raw(*view)
                        .ok_or(DynamicWorkspaceError::UnknownView(*view))
                })
                .collect::<Result<Vec<_>, _>>()?,
            active: *active,
        },
        DocumentDockLayout::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => LayoutSnapshot::Split {
            axis: match axis {
                DocumentSplitAxis::Horizontal => SplitDirection::Horizontal,
                DocumentSplitAxis::Vertical => SplitDirection::Vertical,
            },
            ratio: *ratio,
            first: Box::new(layout_to_guise(first, items)?),
            second: Box::new(layout_to_guise(second, items)?),
        },
    })
}

type ExtensionMap = BTreeMap<String, serde_json::Value>;

fn guise_to_layout(
    snapshot: &LayoutSnapshot,
    items: &RuntimeItemMap,
    occupied: &mut BTreeSet<DockPaneId>,
    next_pane: &mut u64,
    previous_panes: &mut VecDeque<(DockPaneId, ExtensionMap)>,
    previous_splits: &mut VecDeque<ExtensionMap>,
) -> Result<DocumentDockLayout, DynamicWorkspaceError> {
    Ok(match snapshot {
        LayoutSnapshot::Pane {
            items: raw_items,
            active,
        } => {
            let views = raw_items
                .iter()
                .map(|raw| {
                    items
                        .view_from_raw(*raw)
                        .ok_or(DynamicWorkspaceError::UnknownRuntimeItem(*raw))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let (pane_id, extensions) = previous_panes
                .pop_front()
                .filter(|(pane, _)| !occupied.contains(pane))
                .unwrap_or_else(|| {
                    let pane = allocate_pane_id(occupied, next_pane);
                    (pane, BTreeMap::new())
                });
            occupied.insert(pane_id);
            DocumentDockLayout::Pane {
                pane_id,
                items: views,
                active: *active,
                extensions,
            }
        }
        LayoutSnapshot::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let extensions = previous_splits.pop_front().unwrap_or_default();
            DocumentDockLayout::Split {
                axis: match axis {
                    SplitDirection::Horizontal => DocumentSplitAxis::Horizontal,
                    SplitDirection::Vertical => DocumentSplitAxis::Vertical,
                },
                ratio: *ratio,
                first: Box::new(guise_to_layout(
                    first,
                    items,
                    occupied,
                    next_pane,
                    previous_panes,
                    previous_splits,
                )?),
                second: Box::new(guise_to_layout(
                    second,
                    items,
                    occupied,
                    next_pane,
                    previous_panes,
                    previous_splits,
                )?),
                extensions,
            }
        }
    })
}

fn allocate_pane_id(occupied: &BTreeSet<DockPaneId>, next: &mut u64) -> DockPaneId {
    loop {
        let candidate = DockPaneId(*next);
        *next = next.saturating_add(1);
        if candidate.0 != 0 && !occupied.contains(&candidate) {
            return candidate;
        }
    }
}

fn collect_document_panes(layout: &DocumentDockLayout, out: &mut BTreeSet<DockPaneId>) {
    match layout {
        DocumentDockLayout::Pane { pane_id, .. } => {
            out.insert(*pane_id);
        }
        DocumentDockLayout::Split { first, second, .. } => {
            collect_document_panes(first, out);
            collect_document_panes(second, out);
        }
    }
}

fn collect_layout_metadata(
    layout: &DocumentDockLayout,
    panes: &mut VecDeque<(DockPaneId, ExtensionMap)>,
    splits: &mut VecDeque<ExtensionMap>,
) {
    match layout {
        DocumentDockLayout::Pane {
            pane_id,
            extensions,
            ..
        } => panes.push_back((*pane_id, extensions.clone())),
        DocumentDockLayout::Split {
            first,
            second,
            extensions,
            ..
        } => {
            splits.push_back(extensions.clone());
            collect_layout_metadata(first, panes, splits);
            collect_layout_metadata(second, panes, splits);
        }
    }
}

pub fn migrate_legacy_snapshot(
    snapshot: WorkspaceSnapshotDto,
) -> Result<WorkspaceDocument, DynamicWorkspaceError> {
    snapshot.validate()?;
    let guise = LayoutSnapshot::decode(&snapshot.main_layout)
        .map_err(|error| DynamicWorkspaceError::Snapshot(error.to_string()))?;
    let layout = WorkspaceLayout::from_guise(&guise)?;
    let legacy = LegacySixWorkspace {
        main_layout: legacy_layout(layout),
        main_window: snapshot.main_window.map(document_placement_from_legacy),
        floating: snapshot
            .floating
            .into_iter()
            .map(|floating| {
                let view = BuiltinView::from_id(floating.view_id)
                    .ok_or(DynamicWorkspaceError::UnknownLegacyView(floating.view_id))?;
                Ok(LegacyFloatingView {
                    window_id: DocumentWindowId(floating.window_id.0),
                    view: legacy_builtin(view),
                    placement: floating.placement.map(document_placement_from_legacy),
                })
            })
            .collect::<Result<Vec<_>, DynamicWorkspaceError>>()?,
    };
    Ok(WorkspaceDocument::from_legacy_six(legacy)?)
}

fn legacy_layout(layout: WorkspaceLayout) -> LegacySixDockLayout {
    match layout {
        WorkspaceLayout::Pane { items, active } => LegacySixDockLayout::Pane {
            items: items.into_iter().map(legacy_builtin).collect(),
            active,
        },
        WorkspaceLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => LegacySixDockLayout::Split {
            axis: match axis {
                SplitAxis::Horizontal => DocumentSplitAxis::Horizontal,
                SplitAxis::Vertical => DocumentSplitAxis::Vertical,
            },
            ratio,
            first: Box::new(legacy_layout(*first)),
            second: Box::new(legacy_layout(*second)),
        },
    }
}

fn legacy_builtin(view: BuiltinView) -> LegacyBuiltinView {
    match view {
        BuiltinView::Track => LegacyBuiltinView::Track,
        BuiltinView::Waterfall => LegacyBuiltinView::Waterfall,
        BuiltinView::Rhythm => LegacyBuiltinView::Rhythm,
        BuiltinView::Components => LegacyBuiltinView::Components,
        BuiltinView::Separation => LegacyBuiltinView::Separation,
        BuiltinView::Loom => LegacyBuiltinView::Loom,
    }
}

pub fn document_placement_from_gpui(bounds: WindowBounds) -> DocumentWindowPlacement {
    let legacy = WindowPlacementDto::from_gpui(bounds);
    document_placement_from_legacy(legacy)
}

pub fn document_placement_to_gpui(
    placement: DocumentWindowPlacement,
) -> Result<WindowBounds, DynamicWorkspaceError> {
    let legacy = WindowPlacementDto {
        mode: match placement.mode {
            DocumentWindowMode::Windowed => WindowModeDto::Windowed,
            DocumentWindowMode::Maximized => WindowModeDto::Maximized,
            DocumentWindowMode::Fullscreen => WindowModeDto::Fullscreen,
        },
        x: placement.x,
        y: placement.y,
        width: placement.width,
        height: placement.height,
    };
    Ok(legacy.to_gpui()?)
}

fn document_placement_from_legacy(placement: WindowPlacementDto) -> DocumentWindowPlacement {
    DocumentWindowPlacement {
        mode: match placement.mode {
            WindowModeDto::Windowed => DocumentWindowMode::Windowed,
            WindowModeDto::Maximized => DocumentWindowMode::Maximized,
            WindowModeDto::Fullscreen => DocumentWindowMode::Fullscreen,
        },
        x: placement.x,
        y: placement.y,
        width: placement.width,
        height: placement.height,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DynamicWorkspaceError {
    Document(WorkspaceDocumentError),
    Legacy(WorkspaceError),
    Snapshot(String),
    UnknownView(DocumentViewId),
    UnknownRuntimeItem(u64),
    UnknownWindow(DocumentWindowId),
    UnknownLegacyView(WorkspaceViewId),
}

impl fmt::Display for DynamicWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Document(error) => error.fmt(formatter),
            Self::Legacy(error) => error.fmt(formatter),
            Self::Snapshot(message) => write!(formatter, "workspace snapshot: {message}"),
            Self::UnknownView(view) => {
                write!(formatter, "workspace view {} has no runtime item", view.0)
            }
            Self::UnknownRuntimeItem(item) => {
                write!(formatter, "Guise item {item} is not registered")
            }
            Self::UnknownWindow(window) => {
                write!(formatter, "workspace window {} is unknown", window.0)
            }
            Self::UnknownLegacyView(view) => {
                write!(formatter, "legacy workspace view {} is unknown", view.0)
            }
        }
    }
}

impl Error for DynamicWorkspaceError {}

impl From<WorkspaceDocumentError> for DynamicWorkspaceError {
    fn from(error: WorkspaceDocumentError) -> Self {
        Self::Document(error)
    }
}

impl From<WorkspaceError> for DynamicWorkspaceError {
    fn from(error: WorkspaceError) -> Self {
        Self::Legacy(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> WindowPlacementDto {
        WindowPlacementDto {
            mode: WindowModeDto::Windowed,
            x: 120.0,
            y: 80.0,
            width: 960.0,
            height: 640.0,
        }
    }

    #[test]
    fn builtins_have_stable_ids_and_deterministic_guise_ids() {
        assert_eq!(
            BuiltinView::ALL.map(BuiltinView::id),
            [
                WorkspaceViewId(1),
                WorkspaceViewId(2),
                WorkspaceViewId(3),
                WorkspaceViewId(4),
                WorkspaceViewId(5),
                WorkspaceViewId(6),
            ]
        );
        let first = BuiltinItemIds::new();
        let second = BuiltinItemIds::new();
        for view in BuiltinView::ALL {
            assert_eq!(first.item(view), second.item(view));
            assert_eq!(first.view(first.item(view)), Some(view));
        }
    }

    #[test]
    fn default_layout_has_stable_encoded_shape_and_round_trips() {
        let layout = WorkspaceLayout::default_edit();
        layout.validate().unwrap();
        let encoded = layout.to_guise().encode();
        assert_eq!(encoded, "v0.72(p0@1,2,3,4|p0@6,5)");
        let decoded = LayoutSnapshot::decode(&encoded).unwrap();
        assert_eq!(WorkspaceLayout::from_guise(&decoded).unwrap(), layout);
    }

    #[test]
    fn float_and_dock_back_preserve_state_and_are_idempotent() {
        let mut model = WorkspaceModel::new();
        let window = model
            .float_view(BuiltinView::Waterfall, Some(bounds()))
            .unwrap();
        assert_eq!(window, FloatingWindowId(1));
        assert_eq!(
            model.location(BuiltinView::Waterfall),
            ViewLocation::Floating(window)
        );
        assert!(!model.main_layout().contains(BuiltinView::Waterfall));

        assert!(model.dock_back(BuiltinView::Waterfall).unwrap());
        assert_eq!(model.location(BuiltinView::Waterfall), ViewLocation::Docked);
        assert!(!model.dock_back(BuiltinView::Waterfall).unwrap());
        assert_eq!(model.floating().count(), 0);
    }

    #[test]
    fn track_is_pinned() {
        let mut model = WorkspaceModel::new();
        assert_eq!(
            model.float_view(BuiltinView::Track, None),
            Err(WorkspaceError::PinnedView(BuiltinView::Track.id()))
        );
    }

    #[test]
    fn snapshot_json_round_trip_rebuilds_runtime_ids_and_window_sequence() {
        let mut model = WorkspaceModel::new();
        model.set_main_window(Some(bounds())).unwrap();
        model.float_view(BuiltinView::Loom, Some(bounds())).unwrap();
        let first_runtime_item = model.item(BuiltinView::Components);

        let json = model.snapshot().to_json_pretty().unwrap();
        let dto = WorkspaceSnapshotDto::from_json(&json).unwrap();
        let mut restored = WorkspaceModel::from_snapshot(dto).unwrap();
        assert_eq!(restored.snapshot(), model.snapshot());
        assert_eq!(restored.item(BuiltinView::Components), first_runtime_item);
        assert_eq!(
            restored.float_view(BuiltinView::Rhythm, None).unwrap(),
            FloatingWindowId(2)
        );
    }

    #[test]
    fn window_placement_round_trips_gpui_state() {
        let expected = WindowPlacementDto {
            mode: WindowModeDto::Maximized,
            ..bounds()
        };
        let gpui = expected.to_gpui().unwrap();
        assert_eq!(WindowPlacementDto::from_gpui(gpui), expected);
    }

    #[test]
    fn rejects_unknown_duplicate_and_invalid_snapshot_state() {
        let mut duplicate = WorkspaceModel::new().snapshot();
        duplicate.floating.push(FloatingViewDto {
            window_id: FloatingWindowId(1),
            view_id: BuiltinView::Loom.id(),
            placement: None,
        });
        assert_eq!(
            duplicate.validate(),
            Err(WorkspaceError::DuplicateView(BuiltinView::Loom.id()))
        );

        let mut unknown = WorkspaceModel::new().snapshot();
        unknown.main_layout = "p0@1,99".to_owned();
        assert_eq!(
            unknown.validate(),
            Err(WorkspaceError::UnknownView(WorkspaceViewId(99)))
        );

        let mut invalid_bounds = WorkspaceModel::new().snapshot();
        invalid_bounds.main_window = Some(WindowPlacementDto {
            width: 0.0,
            ..bounds()
        });
        assert_eq!(
            invalid_bounds.validate(),
            Err(WorkspaceError::InvalidWindowPlacement)
        );
    }

    #[test]
    fn removing_a_single_item_pane_collapses_its_split() {
        let mut layout = WorkspaceLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(WorkspaceLayout::Pane {
                items: vec![BuiltinView::Track],
                active: 0,
            }),
            second: Box::new(WorkspaceLayout::Pane {
                items: vec![BuiltinView::Loom],
                active: 0,
            }),
        };
        assert!(layout.remove(BuiltinView::Loom));
        assert_eq!(
            layout,
            WorkspaceLayout::Pane {
                items: vec![BuiltinView::Track],
                active: 0,
            }
        );
    }

    #[test]
    fn dynamic_runtime_ids_do_not_depend_on_sparse_durable_ids() {
        let mut document = WorkspaceDocument::default();
        document.close_view(DocumentViewId::WATERFALL).unwrap();
        let model = DynamicWorkspaceModel::new(document).unwrap();
        assert_eq!(model.item_map().raw(DocumentViewId::RHYTHM), Some(2));
        assert_ne!(DocumentViewId::RHYTHM.0, 2);
    }

    #[test]
    fn dynamic_guise_round_trip_preserves_durable_layout_identity() {
        let document = WorkspaceDocument::default();
        let expected = document.main_layout.clone();
        let mut model = DynamicWorkspaceModel::new(document).unwrap();
        let guise = model.main_guise_layout().unwrap();
        model.replace_main_layout(&guise).unwrap();
        assert_eq!(model.document().main_layout, expected);
    }

    #[test]
    fn legacy_snapshot_migrates_into_the_dynamic_document() {
        let legacy = WorkspaceModel::new().snapshot();
        let model = DynamicWorkspaceModel::from_legacy_snapshot(legacy).unwrap();
        assert_eq!(model.document().views.len(), BuiltinView::ALL.len());
        assert_eq!(
            model
                .document()
                .location(DocumentViewId::TRACK_OVERVIEW)
                .unwrap(),
            crate::workspace_document::ViewLocation::Docked
        );
    }
}
