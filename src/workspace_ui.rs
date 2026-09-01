//! GPUI/Guise host for audec's persistent dock, tab, and native-window workspace.
//!
//! This module is intentionally independent of `ui::Workbench` and its private
//! visualizer types. Callers register already-created GPUI entities (or custom
//! render closures) under the stable [`BuiltinView`] identities, then install
//! [`WorkspaceRoot`] as the main window root. A view is never recreated when it
//! moves between the dock and a native floating window.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, size, AnyElement, AnyWindowHandle, App, Bounds, Context, Entity,
    FocusHandle, Focusable, Hsla, Render, ScrollHandle, SharedString, WeakEntity, Window,
    WindowBounds, WindowOptions,
};
use guise::panegroup::{ItemId, PaneId};
use guise::{Button, PaneGroup, PaneGroupEvent, SplitDirection};

use crate::workspace::accessibility::{
    command_for_semantic_action, WorkspaceSemanticAction, WorkspaceSemanticError,
    WorkspaceSemanticNodeId, WorkspaceSemanticTree,
};
use crate::workspace::native_authority::{
    AcceptedWorkspaceCommand, WorkspaceAuthorityError, WorkspaceCommandAuthority,
    WorkspaceLayoutCommand, WorkspaceNativeFailure, WorkspaceNativeOperation, WorkspaceRollback,
};
use crate::workspace::{
    document_placement_from_gpui, document_placement_to_gpui, BuiltinView, DynamicWorkspaceError,
    DynamicWorkspaceModel, FloatingWindowId, RuntimeItemMap, ViewLocation, WindowPlacementDto,
    WorkspaceModel, WorkspaceSnapshotDto,
};
use crate::workspace_document::{
    DockLayout, NewWorkspaceView, ViewLocation as DocumentViewLocation, WindowPlacement,
    WorkspaceDocument, WorkspaceViewDescriptor, WorkspaceViewId as DocumentViewId,
    WorkspaceWindowId as DocumentWindowId,
};
#[cfg(target_os = "macos")]
use crate::workspace_session_layout::{
    default_workspace_titlebar_layout, TitlebarComposition, WindowPlatform,
};
use crate::workspace_session_layout::{
    NativeWindowEffect, PaneBindingEffect, PaneInstanceId, PaneMoveDestination,
    PanePresentationMemory, PaneScrollState, WorkspaceWindow,
};

type PaneRenderer = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type DotRenderer = Rc<dyn Fn(&App) -> Option<Hsla>>;
type ChromeRenderer = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type SnapshotCallback = Rc<dyn Fn(WorkspaceSnapshotDto, &mut App)>;
type EventCallback = Rc<dyn Fn(WorkspaceUiEvent, &mut App)>;
type FloatingOptions =
    Rc<dyn Fn(BuiltinView, Option<WindowPlacementDto>, &mut App) -> WindowOptions>;

/// Overflow contract for pane bodies and workspace chrome rails. It is opt-in
/// because timeline/canvas editors own their own viewport gestures, while
/// browsers, inspectors, and narrow control rails need ordinary native scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceOverflow {
    #[default]
    Clip,
    Horizontal,
    Vertical,
    Both,
}

/// Wrap intrinsically-sized workspace content in a constrained, tracked GPUI
/// scroll region. The handle remains stable when the same pane registration is
/// moved into a split or native floating window.
pub fn workspace_scroll_region(
    id: impl Into<gpui::ElementId>,
    overflow: WorkspaceOverflow,
    handle: &ScrollHandle,
    content: AnyElement,
) -> AnyElement {
    let region = div()
        .id(id)
        .size_full()
        .min_w_0()
        .min_h_0()
        .track_scroll(handle);
    let region = match overflow {
        WorkspaceOverflow::Clip => region.overflow_hidden(),
        WorkspaceOverflow::Horizontal => region.overflow_x_scroll().overflow_y_hidden(),
        WorkspaceOverflow::Vertical => region.overflow_y_scroll().overflow_x_hidden(),
        WorkspaceOverflow::Both => region.overflow_scroll(),
    };
    region.child(content).into_any_element()
}

/// A registered workspace pane. Cloning this value clones the renderer's
/// captured `Entity<T>` handle, never the entity state itself.
#[derive(Clone)]
pub struct PaneRegistration {
    title: SharedString,
    render: PaneRenderer,
    dot: DotRenderer,
    overflow: WorkspaceOverflow,
    scroll: Option<ScrollHandle>,
}

impl PaneRegistration {
    pub fn renderer(
        title: impl Into<SharedString>,
        render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            render: Rc::new(render),
            dot: Rc::new(|_| None),
            overflow: WorkspaceOverflow::Clip,
            scroll: None,
        }
    }

    /// Register a concrete GPUI entity. The same handle is rendered in both
    /// the main PaneGroup and a tear-off window.
    pub fn entity<T>(title: impl Into<SharedString>, entity: Entity<T>) -> Self
    where
        T: Render + 'static,
    {
        Self::renderer(title, move |_window, _cx| entity.clone().into_any_element())
    }

    pub fn with_dot(mut self, dot: impl Fn(&App) -> Option<Hsla> + 'static) -> Self {
        self.dot = Rc::new(dot);
        self
    }

    /// Give this pane ordinary overflow behavior using a stable scroll handle.
    /// Prefer vertical for rails/inspectors and both for large tabular tools.
    pub fn with_overflow(mut self, overflow: WorkspaceOverflow) -> Self {
        self.overflow = overflow;
        self.scroll = (overflow != WorkspaceOverflow::Clip).then(ScrollHandle::new);
        self
    }

    /// Supply a caller-owned handle when chrome and pane code both need to
    /// inspect or restore the same scroll position.
    pub fn with_tracked_overflow(
        mut self,
        overflow: WorkspaceOverflow,
        handle: ScrollHandle,
    ) -> Self {
        self.overflow = overflow;
        self.scroll = Some(handle);
        self
    }

    pub fn scroll_handle(&self) -> Option<ScrollHandle> {
        self.scroll.clone()
    }

    pub fn scroll_state(&self) -> Option<PaneScrollState> {
        self.scroll.as_ref().map(|handle| {
            let offset = handle.offset();
            PaneScrollState {
                horizontal: (-f32::from(offset.x)).max(0.0),
                vertical: (-f32::from(offset.y)).max(0.0),
            }
        })
    }

    pub fn restore_scroll_state(&self, state: PaneScrollState) {
        if let Some(handle) = &self.scroll {
            handle.set_offset(gpui::point(
                px(-state.horizontal.max(0.0)),
                px(-state.vertical.max(0.0)),
            ));
        }
    }

    fn element(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let content = (self.render)(window, cx);
        match &self.scroll {
            Some(handle) => {
                workspace_scroll_region(self.title.clone(), self.overflow, handle, content)
            }
            None => content,
        }
    }
}

/// Stable registry shared by docked and floating renderers.
#[derive(Clone, Default)]
pub struct PaneRegistry {
    entries: BTreeMap<BuiltinView, PaneRegistration>,
}

impl PaneRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, view: BuiltinView, pane: PaneRegistration) -> &mut Self {
        self.entries.insert(view, pane);
        self
    }

    pub fn register_entity<T>(
        &mut self,
        view: BuiltinView,
        title: impl Into<SharedString>,
        entity: Entity<T>,
    ) -> &mut Self
    where
        T: Render + 'static,
    {
        self.register(view, PaneRegistration::entity(title, entity))
    }

    pub fn contains(&self, view: BuiltinView) -> bool {
        self.entries.contains_key(&view)
    }

    pub fn missing_builtins(&self) -> Vec<BuiltinView> {
        BuiltinView::ALL
            .into_iter()
            .filter(|view| !self.contains(*view))
            .collect()
    }

    pub fn scroll_state(&self, view: BuiltinView) -> Option<PaneScrollState> {
        self.entries
            .get(&view)
            .and_then(PaneRegistration::scroll_state)
    }

    pub fn restore_scroll_state(&self, view: BuiltinView, state: PaneScrollState) -> bool {
        self.entries.get(&view).is_some_and(|pane| {
            pane.restore_scroll_state(state);
            pane.scroll_handle().is_some()
        })
    }

    fn get(&self, view: BuiltinView) -> Option<&PaneRegistration> {
        self.entries.get(&view)
    }
}

#[derive(Clone, Debug)]
pub enum WorkspaceUiEvent {
    Activated(BuiltinView),
    CloseDenied(BuiltinView),
    Closed(BuiltinView),
    NewViewRequested(PaneId),
    ContextMenuRequested {
        view: BuiltinView,
        position: gpui::Point<gpui::Pixels>,
    },
    Floated(BuiltinView),
    Docked(BuiltinView),
    LayoutChanged,
    WindowOpenFailed {
        view: BuiltinView,
        message: SharedString,
    },
}

/// Integration callbacks. Persistence remains application-owned so this host
/// can be embedded in a project document without choosing filesystem policy.
#[derive(Clone)]
pub struct WorkspaceHooks {
    on_snapshot: SnapshotCallback,
    on_event: EventCallback,
    floating_options: FloatingOptions,
}

impl Default for WorkspaceHooks {
    fn default() -> Self {
        Self {
            on_snapshot: Rc::new(|_, _| {}),
            on_event: Rc::new(|_, _| {}),
            floating_options: Rc::new(default_floating_options),
        }
    }
}

impl WorkspaceHooks {
    pub fn on_snapshot(
        mut self,
        callback: impl Fn(WorkspaceSnapshotDto, &mut App) + 'static,
    ) -> Self {
        self.on_snapshot = Rc::new(callback);
        self
    }

    pub fn on_event(mut self, callback: impl Fn(WorkspaceUiEvent, &mut App) + 'static) -> Self {
        self.on_event = Rc::new(callback);
        self
    }

    pub fn floating_options(
        mut self,
        callback: impl Fn(BuiltinView, Option<WindowPlacementDto>, &mut App) -> WindowOptions + 'static,
    ) -> Self {
        self.floating_options = Rc::new(callback);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePolicy {
    Allow,
    DenyPinned,
    DenyLastDocked,
}

pub fn close_policy(view: BuiltinView, docked_count: usize) -> ClosePolicy {
    if view == BuiltinView::Track {
        ClosePolicy::DenyPinned
    } else if docked_count <= 1 {
        ClosePolicy::DenyLastDocked
    } else {
        ClosePolicy::Allow
    }
}

pub fn can_tear_off(view: BuiltinView, docked_count: usize) -> bool {
    view != BuiltinView::Track && docked_count > 1
}

#[derive(Clone, Copy)]
struct FloatingRecord {
    id: FloatingWindowId,
    handle: AnyWindowHandle,
}

/// Main-window root: global DAW chrome above a full-size Guise PaneGroup.
pub struct WorkspaceRoot {
    model: WorkspaceModel,
    registry: PaneRegistry,
    panes: Entity<PaneGroup>,
    floating: BTreeMap<BuiltinView, FloatingRecord>,
    chrome: Option<ChromeRenderer>,
    hooks: WorkspaceHooks,
    shutting_down: bool,
}

impl WorkspaceRoot {
    pub fn new(
        model: WorkspaceModel,
        registry: PaneRegistry,
        chrome: Option<impl Fn(&mut Window, &mut App) -> AnyElement + 'static>,
        hooks: WorkspaceHooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let render_registry = registry.clone();
        let title_registry = registry.clone();
        let dot_registry = registry.clone();
        let render_ids = model.item_ids().clone();
        let title_ids = render_ids.clone();
        let dot_ids = render_ids.clone();
        let first = model.item(BuiltinView::Track);
        let initial_layout = model.guise_layout();
        let panes = cx.new(|cx| {
            let group = PaneGroup::new(first, cx).tab_height(30.0);
            #[cfg(target_os = "macos")]
            let group = {
                let safe = default_workspace_titlebar_layout(
                    WindowPlatform::MacOs,
                    TitlebarComposition::OverlayTabs,
                    None,
                )
                .expect("static titlebar metrics are valid");
                let (leading, trailing) = safe.guise_titlebar_insets();
                group.titlebar(leading, trailing)
            };
            group
                .on_render_item(move |item, window, cx| {
                    render_ids
                        .view(item)
                        .and_then(|view| render_registry.get(view))
                        .map(|pane| pane.element(window, cx))
                        .unwrap_or_else(|| missing_pane("This workspace view is not registered"))
                })
                .on_item_title(move |item, _cx| {
                    title_ids
                        .view(item)
                        .and_then(|view| title_registry.get(view).map(|pane| pane.title.clone()))
                        .or_else(|| title_ids.view(item).map(|view| view.title().into()))
                        .unwrap_or_else(|| SharedString::from("Missing view"))
                })
                .on_item_dot(move |item, cx| {
                    dot_ids
                        .view(item)
                        .and_then(|view| dot_registry.get(view))
                        .and_then(|pane| (pane.dot)(cx))
                })
        });
        panes.update(cx, |panes, cx| {
            debug_assert!(panes.restore(&initial_layout, cx));
        });

        cx.subscribe(&panes, |this, panes, event, cx| {
            this.handle_pane_event(panes, event, cx)
        })
        .detach();
        cx.observe(&panes, |this, panes, cx| {
            this.sync_group_layout(&panes, cx);
        })
        .detach();
        cx.observe_window_bounds(window, |this, window, cx| {
            let placement = WindowPlacementDto::from_gpui(window.window_bounds());
            if this.model.set_main_window(Some(placement)).is_ok() {
                this.publish_snapshot(cx);
            }
        })
        .detach();

        let close_root = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            close_root
                .update(cx, |root, cx| {
                    root.shutting_down = true;
                    let placement = WindowPlacementDto::from_gpui(window.window_bounds());
                    let _ = root.model.set_main_window(Some(placement));
                    root.publish_snapshot(cx);
                })
                .ok();
            true
        });

        let restored = model
            .floating()
            .filter_map(|entry| {
                BuiltinView::from_id(entry.view_id)
                    .map(|view| (view, entry.window_id, entry.placement))
            })
            .collect::<Vec<_>>();
        if !restored.is_empty() {
            let workspace = cx.weak_entity();
            cx.defer(move |cx| {
                for (view, id, placement) in restored {
                    workspace
                        .update(cx, |root, cx| {
                            root.open_floating_window(view, id, placement, cx)
                        })
                        .ok();
                }
            });
        }

        Self {
            model,
            registry,
            panes,
            floating: BTreeMap::new(),
            chrome: chrome.map(|render| Rc::new(render) as ChromeRenderer),
            hooks,
            shutting_down: false,
        }
    }

    pub fn panes(&self) -> Entity<PaneGroup> {
        self.panes.clone()
    }

    pub fn pane_scroll_state(&self, view: BuiltinView) -> Option<PaneScrollState> {
        self.registry.scroll_state(view)
    }

    pub fn restore_pane_scroll_state(&self, view: BuiltinView, state: PaneScrollState) -> bool {
        self.registry.restore_scroll_state(view, state)
    }

    pub fn snapshot(&self) -> WorkspaceSnapshotDto {
        self.model.snapshot()
    }

    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    pub fn activate_or_show(&mut self, view: BuiltinView, cx: &mut Context<Self>) {
        match self.model.location(view) {
            ViewLocation::Docked => {
                let item = self.model.item(view);
                self.panes.update(cx, |panes, cx| {
                    if let Some(pane) = panes.pane_of(item) {
                        panes.activate(pane, item, cx);
                    }
                });
            }
            ViewLocation::Floating(_) => {
                if let Some(record) = self.floating.get(&view).copied() {
                    let _ = record
                        .handle
                        .update(cx, |_root, window, _cx| window.activate_window());
                }
            }
            ViewLocation::Hidden => {
                let item = self.model.item(view);
                self.panes
                    .update(cx, |panes, cx| panes.add_to_focused(item, cx));
                self.sync_group_layout_now(cx);
            }
        }
    }

    pub fn close_view(&mut self, view: BuiltinView, cx: &mut Context<Self>) {
        let item = self.model.item(view);
        self.close_item(item, cx);
    }

    pub fn float_view(&mut self, view: BuiltinView, cx: &mut Context<Self>) {
        let item = self.model.item(view);
        let count = self.panes.read(cx).items().len();
        if !can_tear_off(view, count) {
            self.emit(WorkspaceUiEvent::CloseDenied(view), cx);
            return;
        }
        self.panes.update(cx, |panes, cx| panes.tear_off(item, cx));
    }

    pub fn dock_back(&mut self, view: BuiltinView, cx: &mut Context<Self>) {
        self.dock_back_inner(view, true, cx);
    }

    pub fn split_view(
        &mut self,
        view: BuiltinView,
        direction: SplitDirection,
        first: bool,
        cx: &mut Context<Self>,
    ) {
        if self.model.location(view) != ViewLocation::Hidden {
            self.activate_or_show(view, cx);
            return;
        }
        let item = self.model.item(view);
        self.panes.update(cx, |panes, cx| {
            let focused = panes.focused_pane();
            panes.split(focused, direction, first, item, cx);
        });
        self.sync_group_layout_now(cx);
    }

    pub fn equalize_splits(&mut self, cx: &mut Context<Self>) {
        self.panes.update(cx, |panes, cx| panes.equalize(cx));
    }

    pub fn toggle_pane_zoom(&mut self, cx: &mut Context<Self>) {
        self.panes.update(cx, |panes, cx| panes.toggle_zoom(cx));
    }

    pub fn reset_default_layout(&mut self, cx: &mut Context<Self>) {
        let main_window = self.model.main_window();
        let handles = self
            .floating
            .values()
            .map(|record| record.handle)
            .collect::<Vec<_>>();
        self.floating.clear();
        self.model = WorkspaceModel::new();
        let _ = self.model.set_main_window(main_window);
        let layout = self.model.guise_layout();
        self.panes.update(cx, |panes, cx| {
            debug_assert!(panes.restore(&layout, cx));
        });
        for handle in handles {
            let _ = handle.update(cx, |_root, window, _cx| window.remove_window());
        }
        self.publish_snapshot(cx);
        cx.notify();
    }

    fn handle_pane_event(
        &mut self,
        panes: Entity<PaneGroup>,
        event: &PaneGroupEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaneGroupEvent::Activated(item) => {
                if let Some(view) = self.model.view(*item) {
                    self.emit(WorkspaceUiEvent::Activated(view), cx);
                }
            }
            PaneGroupEvent::CloseRequested(item) => self.close_item(*item, cx),
            PaneGroupEvent::NewRequested(pane) => {
                self.emit(WorkspaceUiEvent::NewViewRequested(*pane), cx)
            }
            PaneGroupEvent::FocusChanged(_) => self.sync_group_layout(&panes, cx),
            PaneGroupEvent::TearOff(item) => self.torn_off(*item, cx),
            PaneGroupEvent::ContextMenu { item, position } => {
                if let Some(view) = self.model.view(*item) {
                    self.emit(
                        WorkspaceUiEvent::ContextMenuRequested {
                            view,
                            position: *position,
                        },
                        cx,
                    );
                }
            }
        }
    }

    fn close_item(&mut self, item: ItemId, cx: &mut Context<Self>) {
        let Some(view) = self.model.view(item) else {
            return;
        };
        let policy = close_policy(view, self.panes.read(cx).items().len());
        if policy != ClosePolicy::Allow {
            self.emit(WorkspaceUiEvent::CloseDenied(view), cx);
            return;
        }
        self.panes
            .update(cx, |panes, cx| panes.close_item(item, cx));
        self.sync_group_layout_now(cx);
        self.emit(WorkspaceUiEvent::Closed(view), cx);
    }

    fn torn_off(&mut self, item: ItemId, cx: &mut Context<Self>) {
        let Some(view) = self.model.view(item) else {
            return;
        };
        if view == BuiltinView::Track {
            self.panes
                .update(cx, |panes, cx| panes.add_to_focused(item, cx));
            self.sync_group_layout_now(cx);
            self.emit(WorkspaceUiEvent::CloseDenied(view), cx);
            return;
        }
        let Ok(window_id) = self.model.float_view(view, None) else {
            self.panes
                .update(cx, |panes, cx| panes.add_to_focused(item, cx));
            self.sync_group_layout_now(cx);
            return;
        };
        self.publish_snapshot(cx);
        self.open_floating_window(view, window_id, None, cx);
    }

    fn open_floating_window(
        &mut self,
        view: BuiltinView,
        window_id: FloatingWindowId,
        placement: Option<WindowPlacementDto>,
        cx: &mut Context<Self>,
    ) {
        if self.floating.contains_key(&view) {
            return;
        }
        let Some(pane) = self.registry.get(view).cloned() else {
            self.dock_back_inner(view, false, cx);
            self.emit(
                WorkspaceUiEvent::WindowOpenFailed {
                    view,
                    message: "workspace pane is not registered".into(),
                },
                cx,
            );
            return;
        };
        let workspace = cx.weak_entity();
        let hooks = self.hooks.clone();
        cx.defer(move |cx| {
            let options = (hooks.floating_options)(view, placement, cx);
            let floating_workspace = workspace.clone();
            let result = cx.open_window(options, move |window, cx| {
                let root = cx.new(|cx| {
                    FloatingPane::new(view, window_id, pane, floating_workspace, window, cx)
                });
                window.focus(&root.focus_handle(cx));
                root
            });
            match result {
                Ok(handle) => {
                    workspace
                        .update(cx, |root, cx| {
                            root.floating.insert(
                                view,
                                FloatingRecord {
                                    id: window_id,
                                    handle: handle.into(),
                                },
                            );
                            root.emit(WorkspaceUiEvent::Floated(view), cx);
                            root.publish_snapshot(cx);
                        })
                        .ok();
                }
                Err(error) => {
                    workspace
                        .update(cx, |root, cx| {
                            root.dock_back_inner(view, false, cx);
                            root.emit(
                                WorkspaceUiEvent::WindowOpenFailed {
                                    view,
                                    message: error.to_string().into(),
                                },
                                cx,
                            );
                        })
                        .ok();
                }
            }
        });
    }

    fn dock_back_inner(
        &mut self,
        view: BuiltinView,
        close_native_window: bool,
        cx: &mut Context<Self>,
    ) {
        if self.shutting_down {
            return;
        }
        let Ok(changed) = self.model.dock_back(view) else {
            return;
        };
        let record = self.floating.remove(&view);
        if changed {
            let item = self.model.item(view);
            self.panes
                .update(cx, |panes, cx| panes.add_to_focused(item, cx));
            self.sync_group_layout_now(cx);
            self.emit(WorkspaceUiEvent::Docked(view), cx);
        }
        if close_native_window {
            if let Some(record) = record {
                cx.defer(move |cx| {
                    let _ = record
                        .handle
                        .update(cx, |_root, window, _cx| window.remove_window());
                });
            }
        }
    }

    fn floating_bounds_changed(
        &mut self,
        view: BuiltinView,
        window_id: FloatingWindowId,
        placement: WindowPlacementDto,
        cx: &mut Context<Self>,
    ) {
        if self
            .floating
            .get(&view)
            .is_some_and(|record| record.id != window_id)
        {
            return;
        }
        if self
            .model
            .update_floating_placement(window_id, placement)
            .is_ok()
        {
            self.publish_snapshot(cx);
        }
    }

    fn sync_group_layout_now(&mut self, cx: &mut Context<Self>) {
        let panes = self.panes.clone();
        self.sync_group_layout(&panes, cx);
    }

    fn sync_group_layout(&mut self, panes: &Entity<PaneGroup>, cx: &mut Context<Self>) {
        let snapshot = panes.read(cx).snapshot();
        if self.model.replace_main_layout(&snapshot).is_ok() {
            self.emit(WorkspaceUiEvent::LayoutChanged, cx);
            self.publish_snapshot(cx);
        }
    }

    fn emit(&self, event: WorkspaceUiEvent, cx: &mut App) {
        (self.hooks.on_event)(event, cx);
    }

    fn publish_snapshot(&self, cx: &mut App) {
        (self.hooks.on_snapshot)(self.model.snapshot(), cx);
    }
}

impl Render for WorkspaceRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().size_full().flex().flex_col();
        if let Some(chrome) = &self.chrome {
            root = root.child((chrome)(window, cx));
        }
        root.child(div().flex_1().min_h_0().child(self.panes.clone()))
    }
}

struct FloatingPane {
    view: BuiltinView,
    pane: PaneRegistration,
    workspace: WeakEntity<WorkspaceRoot>,
    focus: FocusHandle,
}

impl FloatingPane {
    fn new(
        view: BuiltinView,
        window_id: FloatingWindowId,
        pane: PaneRegistration,
        workspace: WeakEntity<WorkspaceRoot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let bounds_workspace = workspace.clone();
        cx.observe_window_bounds(window, move |_this, window, cx| {
            let placement = WindowPlacementDto::from_gpui(window.window_bounds());
            bounds_workspace
                .update(cx, |root, cx| {
                    root.floating_bounds_changed(view, window_id, placement, cx)
                })
                .ok();
        })
        .detach();

        let close_workspace = workspace.clone();
        window.on_window_should_close(cx, move |_window, cx| {
            close_workspace
                .update(cx, |root, cx| root.dock_back_inner(view, false, cx))
                .ok();
            true
        });

        Self {
            view,
            pane,
            workspace,
            focus: cx.focus_handle(),
        }
    }
}

impl Focusable for FloatingPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FloatingPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let view = self.view;
        let root = div().size_full().flex().flex_col().track_focus(&self.focus);
        #[cfg(target_os = "macos")]
        let root = root.pt(px(default_workspace_titlebar_layout(
            WindowPlatform::MacOs,
            TitlebarComposition::ContentBelowTitlebar,
            None,
        )
        .expect("static titlebar metrics are valid")
        .content
        .top));
        root.child(
            div()
                .h(px(44.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .child(self.pane.title.clone())
                .child(
                    Button::new(("audec-dock-back", view.id().0), "Dock Back").on_click(
                        move |_event, _window, cx| {
                            workspace
                                .update(cx, |root, cx| root.dock_back(view, cx))
                                .ok();
                        },
                    ),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(self.pane.element(window, cx)),
        )
    }
}

fn missing_pane(message: impl Into<SharedString>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(message.into())
        .into_any_element()
}

pub fn default_floating_options(
    view: BuiltinView,
    placement: Option<WindowPlacementDto>,
    cx: &mut App,
) -> WindowOptions {
    let bounds = placement
        .and_then(|placement| placement.to_gpui().ok())
        .unwrap_or_else(|| {
            WindowBounds::Windowed(Bounds::centered(None, size(px(1_080.0), px(720.0)), cx))
        });
    WindowOptions {
        window_bounds: Some(bounds),
        window_min_size: Some(size(px(560.0), px(360.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(format!("audec — {}", view.title()).into()),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

// -------------------------------------------------------------------------
// Dynamic workspace v2

type DynamicPaneFactory =
    Rc<dyn Fn(&WorkspaceViewDescriptor, &mut App) -> Result<PaneRegistration, SharedString>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingWorkspaceViewDiagnostic {
    pub view: DocumentViewId,
    pub message: SharedString,
}

/// Shared instance registry for the dynamic workspace. Durable view IDs and
/// runtime Guise item IDs are both lookups into this registry; neither is used
/// as a project-domain identity.
#[derive(Clone, Default)]
pub struct DynamicPaneRegistry {
    entries: Rc<RefCell<BTreeMap<DocumentViewId, PaneRegistration>>>,
    descriptors: Rc<RefCell<BTreeMap<DocumentViewId, WorkspaceViewDescriptor>>>,
    runtime_views: Rc<RefCell<BTreeMap<ItemId, DocumentViewId>>>,
    raw_items: Rc<RefCell<BTreeMap<u64, ItemId>>>,
    factory: Rc<RefCell<Option<DynamicPaneFactory>>>,
    missing: Rc<RefCell<BTreeMap<DocumentViewId, SharedString>>>,
}

impl DynamicPaneRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_factory(
        self,
        factory: impl Fn(&WorkspaceViewDescriptor, &mut App) -> Result<PaneRegistration, SharedString>
            + 'static,
    ) -> Self {
        *self.factory.borrow_mut() = Some(Rc::new(factory));
        self
    }

    pub fn register(&self, view: DocumentViewId, pane: PaneRegistration) {
        self.entries.borrow_mut().insert(view, pane);
        self.missing.borrow_mut().remove(&view);
    }

    pub fn register_entity<T>(
        &self,
        view: DocumentViewId,
        title: impl Into<SharedString>,
        entity: Entity<T>,
    ) where
        T: Render + 'static,
    {
        self.register(view, PaneRegistration::entity(title, entity));
    }

    pub fn scroll_state(&self, view: DocumentViewId) -> Option<PaneScrollState> {
        self.entries
            .borrow()
            .get(&view)
            .and_then(PaneRegistration::scroll_state)
    }

    pub fn restore_scroll_state(&self, view: DocumentViewId, state: PaneScrollState) -> bool {
        self.entries.borrow().get(&view).is_some_and(|pane| {
            pane.restore_scroll_state(state);
            pane.scroll_handle().is_some()
        })
    }

    pub fn bind_runtime(&self, view: DocumentViewId, item: ItemId) {
        self.runtime_views.borrow_mut().insert(item, view);
    }

    pub fn bind_all(&self, items: &RuntimeItemMap) {
        let mut runtime = self.runtime_views.borrow_mut();
        runtime.clear();
        runtime.extend(items.bindings().map(|(view, item)| (item, view)));
        let mut raw_items = self.raw_items.borrow_mut();
        raw_items.clear();
        raw_items.extend(items.runtime_bindings().map(|(raw, _, item)| (raw, item)));
    }

    pub fn ensure(
        &self,
        descriptor: &WorkspaceViewDescriptor,
        cx: &mut App,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if self.entries.borrow().contains_key(&descriptor.id) {
            let retained = self.descriptors.borrow().get(&descriptor.id).cloned();
            match retained {
                Some(retained) if retained == *descriptor => {
                    self.missing.borrow_mut().remove(&descriptor.id);
                    return Ok(());
                }
                // Legacy-six entities are installed before their migrated v2
                // descriptors exist. Adopt that first descriptor without
                // recreating a pane which is already live.
                None => {
                    self.descriptors
                        .borrow_mut()
                        .insert(descriptor.id, descriptor.clone());
                    self.missing.borrow_mut().remove(&descriptor.id);
                    return Ok(());
                }
                Some(_) => {}
            }
        }
        let factory = self.factory.borrow().clone();
        let Some(factory) = factory else {
            return Err(DynamicWorkspaceUiError::MissingFactory(descriptor.id));
        };
        let registration =
            factory(descriptor, cx).map_err(|message| DynamicWorkspaceUiError::FactoryFailed {
                view: descriptor.id,
                message,
            })?;
        self.entries
            .borrow_mut()
            .insert(descriptor.id, registration);
        self.descriptors
            .borrow_mut()
            .insert(descriptor.id, descriptor.clone());
        self.missing.borrow_mut().remove(&descriptor.id);
        Ok(())
    }

    /// Materialize every recoverable view while retaining unavailable
    /// descriptors as visible placeholders. One missing extension/factory
    /// must not prevent the rest of a project workspace from opening.
    pub fn reconcile_restored_document(
        &self,
        document: &WorkspaceDocument,
        cx: &mut App,
    ) -> Vec<MissingWorkspaceViewDiagnostic> {
        let retained = document.views.keys().copied().collect::<BTreeSet<_>>();
        let stale = self
            .entries
            .borrow()
            .keys()
            .chain(self.descriptors.borrow().keys())
            .copied()
            .filter(|view| !retained.contains(view))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for view in stale {
            self.remove(view);
        }

        let mut diagnostics = Vec::new();
        for descriptor in document.views.values() {
            if let Err(error) = self.ensure(descriptor, cx) {
                let message = SharedString::from(error.to_string());
                let changed_target = self
                    .descriptors
                    .borrow()
                    .get(&descriptor.id)
                    .is_some_and(|retained| retained != descriptor);
                if changed_target {
                    // Showing the previous entity under a newly restored
                    // target would be worse than an explicit placeholder.
                    self.entries.borrow_mut().remove(&descriptor.id);
                }
                self.descriptors
                    .borrow_mut()
                    .insert(descriptor.id, descriptor.clone());
                self.missing
                    .borrow_mut()
                    .insert(descriptor.id, message.clone());
                diagnostics.push(MissingWorkspaceViewDiagnostic {
                    view: descriptor.id,
                    message,
                });
            }
        }
        diagnostics
    }

    pub fn missing_view_diagnostics(&self) -> Vec<MissingWorkspaceViewDiagnostic> {
        self.missing
            .borrow()
            .iter()
            .map(|(&view, message)| MissingWorkspaceViewDiagnostic {
                view,
                message: message.clone(),
            })
            .collect()
    }

    /// Reconcile runtime entities with a newly imported portable document.
    /// Equal descriptors retain their pane-local state; removed descriptors
    /// release their callbacks/entities, and changed targets are recreated by
    /// the application factory rather than keeping a stale project source.
    pub fn reconcile_document(
        &self,
        document: &WorkspaceDocument,
        cx: &mut App,
    ) -> Result<(), DynamicWorkspaceUiError> {
        let retained = document.views.keys().copied().collect::<BTreeSet<_>>();
        let stale = self
            .entries
            .borrow()
            .keys()
            .copied()
            .filter(|view| !retained.contains(view))
            .collect::<Vec<_>>();
        for view in stale {
            self.remove(view);
        }
        for descriptor in document.views.values() {
            self.ensure(descriptor, cx)?;
        }
        Ok(())
    }

    pub fn remove(&self, view: DocumentViewId) {
        self.entries.borrow_mut().remove(&view);
        self.descriptors.borrow_mut().remove(&view);
        self.missing.borrow_mut().remove(&view);
        self.runtime_views
            .borrow_mut()
            .retain(|_, registered| *registered != view);
        self.raw_items
            .borrow_mut()
            .retain(|_, item| self.runtime_views.borrow().contains_key(item));
    }

    fn view(&self, item: ItemId) -> Option<DocumentViewId> {
        self.runtime_views.borrow().get(&item).copied()
    }

    fn pane(&self, view: DocumentViewId) -> Option<PaneRegistration> {
        self.entries.borrow().get(&view).cloned()
    }

    fn pane_for_item(&self, item: ItemId) -> Option<PaneRegistration> {
        self.view(item).and_then(|view| self.pane(view))
    }

    fn missing_message_for_item(&self, item: ItemId) -> SharedString {
        self.view(item)
            .and_then(|view| self.missing.borrow().get(&view).cloned())
            .unwrap_or_else(|| "This workspace view is not registered".into())
    }

    fn title_for_item(&self, item: ItemId) -> SharedString {
        let Some(view) = self.view(item) else {
            return "Missing view".into();
        };
        self.pane(view)
            .map(|pane| pane.title)
            .or_else(|| {
                self.descriptors
                    .borrow()
                    .get(&view)
                    .and_then(|descriptor| descriptor.title_override.clone())
                    .map(SharedString::from)
            })
            .unwrap_or_else(|| format!("Unavailable view {}", view.0).into())
    }

    /// Bridge the original six registrations into v2 without recreating any
    /// editor entities. This is the migration path used by the first app-shell
    /// convergence pass.
    pub fn register_legacy_six(&self, legacy: &PaneRegistry) {
        let bindings = [
            (BuiltinView::Track, DocumentViewId::TRACK_OVERVIEW),
            (BuiltinView::Waterfall, DocumentViewId::WATERFALL),
            (BuiltinView::Rhythm, DocumentViewId::RHYTHM),
            (BuiltinView::Components, DocumentViewId::COMPONENTS),
            (BuiltinView::Separation, DocumentViewId::SEPARATION),
            (BuiltinView::Loom, DocumentViewId::LOOM),
        ];
        for (builtin, view) in bindings {
            if let Some(pane) = legacy.get(builtin) {
                self.register(view, pane.clone());
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum DynamicWorkspaceUiEvent {
    Activated(DocumentViewId),
    CloseDenied {
        view: DocumentViewId,
        message: SharedString,
    },
    Closed(DocumentViewId),
    Removed(DocumentViewId),
    NewViewRequested {
        window: Option<DocumentWindowId>,
        pane: PaneId,
    },
    ContextMenuRequested {
        view: DocumentViewId,
        position: gpui::Point<gpui::Pixels>,
    },
    Floated {
        view: DocumentViewId,
        window: DocumentWindowId,
    },
    Docked(DocumentViewId),
    LayoutChanged {
        window: Option<DocumentWindowId>,
    },
    WindowOpenFailed {
        view: DocumentViewId,
        message: SharedString,
    },
    NativeActuationFailed {
        operation: &'static str,
        message: SharedString,
        recovery_diagnostics: Vec<SharedString>,
    },
}

type DynamicSnapshotCallback = Rc<dyn Fn(WorkspaceDocument, &mut App)>;
type DynamicEventCallback = Rc<dyn Fn(DynamicWorkspaceUiEvent, &mut App)>;
type DynamicBindingCallback = Rc<dyn Fn(PaneBindingEffect, &mut App) -> Result<(), SharedString>>;
type ProjectWindowCloseCallback = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

#[derive(Clone)]
pub struct DynamicWorkspaceHooks {
    on_snapshot: DynamicSnapshotCallback,
    on_event: DynamicEventCallback,
    on_binding: DynamicBindingCallback,
    on_project_window_close: ProjectWindowCloseCallback,
}

impl Default for DynamicWorkspaceHooks {
    fn default() -> Self {
        Self {
            on_snapshot: Rc::new(|_, _| {}),
            on_event: Rc::new(|_, _| {}),
            on_binding: Rc::new(|_, _| {
                Err("project-session pane binding actuator is not installed".into())
            }),
            // audec is a document application, not a menu-bar agent. A host
            // with several project windows may override this and quit only
            // after its ApplicationController detaches the last one.
            on_project_window_close: Rc::new(|_window, cx| {
                cx.quit();
                true
            }),
        }
    }
}

impl DynamicWorkspaceHooks {
    pub fn on_snapshot(mut self, callback: impl Fn(WorkspaceDocument, &mut App) + 'static) -> Self {
        self.on_snapshot = Rc::new(callback);
        self
    }

    pub fn on_event(
        mut self,
        callback: impl Fn(DynamicWorkspaceUiEvent, &mut App) + 'static,
    ) -> Self {
        self.on_event = Rc::new(callback);
        self
    }

    /// Apply attach/detach effects to the one project session. The default is
    /// Authority-enabled hosts must install this; legacy bootstraps never call
    /// it. Refusing by default prevents a close/reopen command from completing
    /// while its project-session attachment silently remains stale.
    pub fn on_binding_effect(
        mut self,
        callback: impl Fn(PaneBindingEffect, &mut App) -> Result<(), SharedString> + 'static,
    ) -> Self {
        self.on_binding = Rc::new(callback);
        self
    }

    pub fn on_project_window_close(
        mut self,
        callback: impl Fn(&mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_project_window_close = Rc::new(callback);
        self
    }
}

#[derive(Clone)]
struct DynamicFloatingRecord {
    handle: AnyWindowHandle,
    panes: Entity<PaneGroup>,
}

/// One-shot app-shell handoff. The current `ui::create_workspace` can build
/// its six existing entities and legacy `WorkspaceModel` exactly as before,
/// then pass them here. Subsequent panes should be created by `with_factory`
/// from target-bearing v2 descriptors.
pub struct DynamicWorkspaceBootstrap {
    model: DynamicWorkspaceModel,
    registry: DynamicPaneRegistry,
    authority: Option<WorkspaceCommandAuthority>,
}

impl DynamicWorkspaceBootstrap {
    pub fn from_legacy_six(
        model: WorkspaceModel,
        panes: PaneRegistry,
    ) -> Result<Self, DynamicWorkspaceUiError> {
        let model = DynamicWorkspaceModel::from_legacy_snapshot(model.snapshot())?;
        let registry = DynamicPaneRegistry::new();
        registry.register_legacy_six(&panes);
        Ok(Self {
            model,
            registry,
            authority: None,
        })
    }

    pub fn from_document(document: WorkspaceDocument) -> Result<Self, DynamicWorkspaceUiError> {
        Ok(Self {
            model: DynamicWorkspaceModel::new(document)?,
            registry: DynamicPaneRegistry::new(),
            authority: None,
        })
    }

    /// Install project-session layout truth before building GPUI panes. Once
    /// present, callers issue typed commands through `DynamicWorkspaceRoot`
    /// and actuate only the returned accepted effects.
    pub fn with_session_layout(
        mut self,
        layout: crate::workspace_session_layout::WorkspaceSessionLayout,
    ) -> Result<Self, DynamicWorkspaceUiError> {
        self.model
            .replace_document_preserving_runtime(layout.document().clone())?;
        self.authority = Some(WorkspaceCommandAuthority::new(layout));
        Ok(self)
    }

    pub fn with_factory(
        mut self,
        factory: impl Fn(&WorkspaceViewDescriptor, &mut App) -> Result<PaneRegistration, SharedString>
            + 'static,
    ) -> Self {
        self.registry = self.registry.with_factory(factory);
        self
    }

    pub fn register(&self, view: DocumentViewId, pane: PaneRegistration) {
        self.registry.register(view, pane);
    }

    pub fn document(&self) -> &WorkspaceDocument {
        self.model.document()
    }

    pub fn build(
        self,
        chrome: Option<impl Fn(&mut Window, &mut App) -> AnyElement + 'static>,
        hooks: DynamicWorkspaceHooks,
        window: &mut Window,
        cx: &mut Context<DynamicWorkspaceRoot>,
    ) -> Result<DynamicWorkspaceRoot, DynamicWorkspaceUiError> {
        DynamicWorkspaceRoot::new(
            self.model,
            self.registry,
            self.authority,
            chrome,
            hooks,
            window,
            cx,
        )
    }
}

/// Main root for arbitrary editor/lens instances. It owns one dynamic
/// document adapter and presents its main tree plus every native floating
/// tree. Pane registrations keep the same editor entity alive while the view
/// moves between windows.
pub struct DynamicWorkspaceRoot {
    model: DynamicWorkspaceModel,
    authority: Option<WorkspaceCommandAuthority>,
    registry: DynamicPaneRegistry,
    panes: Entity<PaneGroup>,
    main_window: AnyWindowHandle,
    floating: BTreeMap<DocumentWindowId, DynamicFloatingRecord>,
    chrome: Option<ChromeRenderer>,
    hooks: DynamicWorkspaceHooks,
    shutting_down: bool,
    actuating_authority: bool,
}

impl DynamicWorkspaceRoot {
    pub fn new(
        model: DynamicWorkspaceModel,
        registry: DynamicPaneRegistry,
        authority: Option<WorkspaceCommandAuthority>,
        chrome: Option<impl Fn(&mut Window, &mut App) -> AnyElement + 'static>,
        hooks: DynamicWorkspaceHooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self, DynamicWorkspaceUiError> {
        registry.bind_all(model.item_map());
        registry.reconcile_restored_document(model.document(), cx);
        if let Some(authority) = &authority {
            for pane in authority.layout().pane_ids() {
                if let Some(memory) = authority.layout().presentation_memory(pane) {
                    registry.restore_scroll_state(pane.0, memory.scroll);
                }
            }
        }

        let main_layout = model.main_guise_layout()?;
        let panes = create_dynamic_group(&main_layout, &registry, cx)?;
        cx.subscribe(&panes, |this, panes, event, cx| {
            this.handle_group_event(None, panes, event, cx)
        })
        .detach();
        cx.observe(&panes, |this, panes, cx| {
            this.sync_layout(None, &panes.read(cx).snapshot(), cx)
        })
        .detach();
        cx.observe_window_bounds(window, |this, window, cx| {
            if this.actuating_authority {
                return;
            }
            let placement = document_placement_from_gpui(window.window_bounds());
            this.record_window_placement(WorkspaceWindow::Main, Some(placement), cx);
        })
        .detach();

        let close_root = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            close_root
                .update(cx, |root, cx| {
                    if !(root.hooks.on_project_window_close)(window, cx) {
                        return false;
                    }
                    root.shutting_down = true;
                    let placement = document_placement_from_gpui(window.window_bounds());
                    root.record_window_placement(WorkspaceWindow::Main, Some(placement), cx);
                    true
                })
                .unwrap_or(false)
        });

        let restored = model
            .document()
            .floating_windows
            .values()
            .map(|window| (window.id, window.placement))
            .collect::<Vec<_>>();
        if !restored.is_empty() {
            let workspace = cx.weak_entity();
            cx.defer(move |cx| {
                for (window, placement) in restored {
                    workspace
                        .update(cx, |root, cx| {
                            root.restore_floating_window(window, placement, cx)
                        })
                        .ok();
                }
            });
        }

        Ok(Self {
            model,
            authority,
            registry,
            panes,
            main_window: window.window_handle(),
            floating: BTreeMap::new(),
            chrome: chrome.map(|render| Rc::new(render) as ChromeRenderer),
            hooks,
            shutting_down: false,
            actuating_authority: false,
        })
    }

    pub fn document(&self) -> &WorkspaceDocument {
        self.model.document()
    }

    pub fn export_document(&self) -> WorkspaceDocument {
        self.authority
            .as_ref()
            .and_then(|authority| authority.export_document().ok())
            .unwrap_or_else(|| self.model.export_document())
    }

    pub fn authority_revision(&self) -> Option<u64> {
        self.authority
            .as_ref()
            .map(WorkspaceCommandAuthority::revision)
    }

    /// Stable role/name/state/action snapshot for keyboard, menu, test, and
    /// future native-AX bridges. GPUI 0.2.2 itself cannot emit native AX nodes.
    pub fn semantic_tree(&self) -> Option<WorkspaceSemanticTree> {
        self.authority
            .as_ref()
            .map(|authority| WorkspaceSemanticTree::from_layout(authority.layout()))
    }

    pub fn missing_view_diagnostics(&self) -> Vec<MissingWorkspaceViewDiagnostic> {
        self.registry.missing_view_diagnostics()
    }

    /// Retry placeholder materialization after an extension/plugin becomes
    /// available. Successfully restored panes keep the same durable ID and
    /// Guise item, so their tab position does not jump.
    pub fn retry_missing_views(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<MissingWorkspaceViewDiagnostic> {
        let document = self.model.document().clone();
        let diagnostics = self.registry.reconcile_restored_document(&document, cx);
        cx.notify();
        diagnostics
    }

    pub fn command_for_semantic_action(
        &self,
        node: WorkspaceSemanticNodeId,
        action: WorkspaceSemanticAction,
    ) -> Result<WorkspaceLayoutCommand, DynamicWorkspaceUiError> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(DynamicWorkspaceUiError::PortableAuthorityNotInstalled)?;
        command_for_semantic_action(authority.layout(), node, action).map_err(Into::into)
    }

    /// Execute one layout command through the portable authority, then apply
    /// its PaneGroup, binding and native-window effects privately. Native
    /// failure restores the previous document and reconciles every effect
    /// before returning a diagnostic.
    pub fn execute_layout_command(
        &mut self,
        expected_revision: u64,
        command: WorkspaceLayoutCommand,
        cx: &mut Context<Self>,
    ) -> Result<AcceptedWorkspaceCommand, DynamicWorkspaceUiError> {
        let presentation_only = matches!(
            &command,
            WorkspaceLayoutCommand::UpdatePresentationMemory { .. }
        );
        let mut authority = self
            .authority
            .take()
            .ok_or(DynamicWorkspaceUiError::PortableAuthorityNotInstalled)?;
        let before_model = self.model.clone();
        let accepted = match authority.accept(expected_revision, command) {
            Ok(accepted) => accepted,
            Err(error) => {
                self.authority = Some(authority);
                return Err(error.into());
            }
        };

        self.actuating_authority = true;
        let actuation = if presentation_only {
            self.model
                .replace_document_preserving_runtime(accepted.document.clone())
                .map_err(|error| WorkspaceNativeFailure {
                    effect_index: 0,
                    operation: WorkspaceNativeOperation::ApplyDocument,
                    message: error.to_string(),
                })
        } else {
            self.actuate_accepted(&accepted, cx)
        };
        if let Err(failure) = actuation {
            let rollback = match authority.fail(accepted.token, failure.clone()) {
                Ok(rollback) => rollback,
                Err(error) => {
                    self.model = before_model;
                    self.actuating_authority = false;
                    self.authority = Some(authority);
                    return Err(error.into());
                }
            };
            // Preserve the exact process-local Guise item map as well as the
            // durable document when native actuation fails.
            self.model = before_model;
            let recovery_diagnostics = self.reconcile_rollback(&rollback, cx);
            self.actuating_authority = false;
            self.authority = Some(authority);
            self.publish_document(cx);
            let event = DynamicWorkspaceUiEvent::NativeActuationFailed {
                operation: failure.operation.as_str(),
                message: failure.message.clone().into(),
                recovery_diagnostics: recovery_diagnostics
                    .iter()
                    .cloned()
                    .map(SharedString::from)
                    .collect(),
            };
            self.emit(event, cx);
            return Err(DynamicWorkspaceUiError::NativeActuation {
                failure,
                recovery_diagnostics,
            });
        }

        if let Err(error) = authority.complete(accepted.token) {
            self.actuating_authority = false;
            self.authority = Some(authority);
            return Err(error.into());
        }
        self.actuating_authority = false;
        self.authority = Some(authority);
        self.publish_document(cx);
        cx.notify();
        Ok(accepted)
    }

    /// One-call keyboard/menu/AX adapter: semantic intent is resolved against
    /// the current portable revision and executed through the same authority.
    pub fn execute_semantic_action(
        &mut self,
        node: WorkspaceSemanticNodeId,
        action: WorkspaceSemanticAction,
        cx: &mut Context<Self>,
    ) -> Result<AcceptedWorkspaceCommand, DynamicWorkspaceUiError> {
        let command = self.command_for_semantic_action(node, action)?;
        let revision = self
            .authority_revision()
            .ok_or(DynamicWorkspaceUiError::PortableAuthorityNotInstalled)?;
        self.execute_layout_command(revision, command, cx)
    }

    fn actuate_accepted(
        &mut self,
        accepted: &AcceptedWorkspaceCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), WorkspaceNativeFailure> {
        self.apply_authoritative_document(&accepted.document, cx)
            .map_err(|error| WorkspaceNativeFailure {
                effect_index: 0,
                operation: WorkspaceNativeOperation::ApplyDocument,
                message: error.to_string(),
            })?;
        for (index, effect) in accepted.transition.bindings.iter().copied().enumerate() {
            (self.hooks.on_binding)(effect, cx).map_err(|error| WorkspaceNativeFailure {
                effect_index: index,
                operation: WorkspaceNativeOperation::ApplyBinding,
                message: error.to_string(),
            })?;
        }
        for (index, effect) in accepted.transition.windows.iter().copied().enumerate() {
            self.apply_native_window_effect(effect, cx)
                .map_err(|error| WorkspaceNativeFailure {
                    effect_index: index,
                    operation: WorkspaceNativeOperation::ApplyWindow,
                    message: error.to_string(),
                })?;
        }
        Ok(())
    }

    fn reconcile_rollback(
        &mut self,
        rollback: &WorkspaceRollback,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let mut diagnostics = Vec::new();
        if let Err(error) = self.apply_authoritative_document(&rollback.document, cx) {
            diagnostics.push(format!("restore_document: {error}"));
        }
        for (index, effect) in rollback.bindings.iter().copied().enumerate() {
            if let Err(error) = (self.hooks.on_binding)(effect, cx) {
                diagnostics.push(format!("restore_binding[{index}]: {error}"));
            }
        }
        for (index, effect) in rollback.windows.iter().copied().enumerate() {
            if let Err(error) = self.apply_native_window_effect(effect, cx) {
                diagnostics.push(format!("restore_window[{index}]: {error}"));
            }
        }
        diagnostics
    }

    fn apply_authoritative_document(
        &mut self,
        document: &WorkspaceDocument,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        self.model
            .replace_document_preserving_runtime(document.clone())?;
        self.registry.bind_all(self.model.item_map());
        let main = self.model.main_guise_layout()?;
        let restored = self.panes.update(cx, |panes, cx| panes.restore(&main, cx));
        if !restored {
            return Err(DynamicWorkspaceUiError::NativeLayoutRejected { window: None });
        }

        let existing = self
            .floating
            .iter()
            .map(|(&window, record)| (window, record.panes.clone()))
            .collect::<Vec<_>>();
        for (window, panes) in existing {
            if !document.floating_windows.contains_key(&window) {
                continue;
            }
            let layout = self.model.floating_guise_layout(window)?;
            let restored = panes.update(cx, |panes, cx| panes.restore(&layout, cx));
            if !restored {
                return Err(DynamicWorkspaceUiError::NativeLayoutRejected {
                    window: Some(window),
                });
            }
        }
        Ok(())
    }

    fn restore_portable_surface_before_input(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        let document = self
            .authority
            .as_ref()
            .ok_or(DynamicWorkspaceUiError::PortableAuthorityNotInstalled)?
            .export_document()?;
        self.actuating_authority = true;
        let result = self.apply_authoritative_document(&document, cx);
        self.actuating_authority = false;
        result
    }

    fn apply_native_window_effect(
        &mut self,
        effect: NativeWindowEffect,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        match effect {
            NativeWindowEffect::Open { window, placement } => {
                self.try_open_floating_window(window, placement, cx)
            }
            NativeWindowEffect::Close { window } => {
                self.close_native_window(window, cx);
                Ok(())
            }
            NativeWindowEffect::Focus { window, pane } => {
                let item = self
                    .model
                    .item(pane.0)
                    .ok_or(DynamicWorkspaceUiError::UnknownView(pane.0))?;
                match window {
                    WorkspaceWindow::Main => self.panes.update(cx, |panes, cx| {
                        if let Some(dock_pane) = panes.pane_of(item) {
                            panes.activate(dock_pane, item, cx);
                        }
                    }),
                    WorkspaceWindow::Floating(window) => {
                        let record = self
                            .floating
                            .get(&window)
                            .ok_or(DynamicWorkspaceUiError::MissingNativeWindow(window))?;
                        record.panes.update(cx, |panes, cx| {
                            if let Some(dock_pane) = panes.pane_of(item) {
                                panes.activate(dock_pane, item, cx);
                            }
                        });
                        record
                            .handle
                            .update(cx, |_root, window, _cx| window.activate_window())
                            .map_err(|error| DynamicWorkspaceUiError::NativeWindow {
                                operation: "focus_window",
                                message: error.to_string().into(),
                            })?;
                    }
                }
                if matches!(window, WorkspaceWindow::Main) {
                    self.main_window
                        .update(cx, |_root, window, _cx| window.activate_window())
                        .map_err(|error| DynamicWorkspaceUiError::NativeWindow {
                            operation: "focus_main_window",
                            message: error.to_string().into(),
                        })?;
                }
                Ok(())
            }
        }
    }

    /// Atomically replace the portable workspace presentation after opening a
    /// project. Existing pane entities are reused by durable view identity;
    /// newly described instances are materialized through the registry
    /// factory, and stale native floating handles are closed.
    pub fn import_document(
        &mut self,
        document: WorkspaceDocument,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if let Some(revision) = self.authority_revision() {
            document.validate()?;
            let previous_views = self
                .model
                .document()
                .views
                .keys()
                .copied()
                .collect::<BTreeSet<_>>();
            self.registry.reconcile_restored_document(&document, cx);
            match self.execute_layout_command(
                revision,
                WorkspaceLayoutCommand::ReplaceDocument { document },
                cx,
            ) {
                Ok(_) => {
                    self.registry
                        .reconcile_restored_document(self.model.document(), cx);
                    self.registry.bind_all(self.model.item_map());
                    cx.notify();
                    return Ok(());
                }
                Err(error) => {
                    let added = self
                        .registry
                        .descriptors
                        .borrow()
                        .keys()
                        .copied()
                        .filter(|view| !previous_views.contains(view))
                        .collect::<Vec<_>>();
                    for view in added {
                        self.registry.remove(view);
                    }
                    return Err(error);
                }
            }
        }
        let next_authority = self
            .authority
            .as_ref()
            .map(|authority| {
                crate::workspace_session_layout::WorkspaceSessionLayout::from_document(
                    authority.layout().session_id(),
                    document.clone(),
                )
                .map(WorkspaceCommandAuthority::new)
            })
            .transpose()
            .map_err(WorkspaceAuthorityError::from)?;
        let authoritative_document = next_authority
            .as_ref()
            .map(|authority| authority.document().clone())
            .unwrap_or(document);
        let next = DynamicWorkspaceModel::new(authoritative_document)?;
        self.registry
            .reconcile_restored_document(next.document(), cx);
        self.registry.bind_all(next.item_map());
        let layout = next.main_guise_layout()?;
        let handles = self
            .floating
            .values()
            .map(|record| record.handle)
            .collect::<Vec<_>>();
        self.floating.clear();
        self.model = next;
        self.authority = next_authority;
        self.panes.update(cx, |panes, cx| {
            let _ = panes.restore(&layout, cx);
        });
        for handle in handles {
            let _ = handle.update(cx, |_root, window, _cx| window.remove_window());
        }
        let restored = self
            .model
            .document()
            .floating_windows
            .values()
            .map(|window| (window.id, window.placement))
            .collect::<Vec<_>>();
        for (window, placement) in restored {
            self.restore_floating_window(window, placement, cx);
        }
        self.publish_document(cx);
        cx.notify();
        Ok(())
    }

    pub fn pane_group(&self) -> Entity<PaneGroup> {
        self.panes.clone()
    }

    pub fn pane_scroll_state(&self, view: DocumentViewId) -> Option<PaneScrollState> {
        self.registry.scroll_state(view)
    }

    pub fn restore_pane_scroll_state(&self, view: DocumentViewId, state: PaneScrollState) -> bool {
        self.registry.restore_scroll_state(view, state)
    }

    /// Persist the live GPUI scroll handle into the typed session metadata
    /// without rebuilding the dock tree or recreating the pane entity.
    pub fn persist_pane_presentation(
        &mut self,
        view: DocumentViewId,
        focus_region: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        let revision = self
            .authority_revision()
            .ok_or(DynamicWorkspaceUiError::PortableAuthorityNotInstalled)?;
        let layout = self
            .authority
            .as_ref()
            .expect("authority revision checked above")
            .layout();
        let pane = PaneInstanceId(view);
        let mut memory = layout
            .presentation_memory(pane)
            .cloned()
            .unwrap_or_default();
        if let Some(scroll) = self.registry.scroll_state(view) {
            memory.scroll = scroll;
        }
        memory.focus_region = focus_region;
        self.execute_layout_command(
            revision,
            WorkspaceLayoutCommand::UpdatePresentationMemory { pane, memory },
            cx,
        )?;
        Ok(())
    }

    fn record_window_placement(
        &mut self,
        window: WorkspaceWindow,
        placement: Option<WindowPlacement>,
        cx: &mut Context<Self>,
    ) {
        if let Some(revision) = self.authority_revision() {
            let _ = self.execute_layout_command(
                revision,
                WorkspaceLayoutCommand::SetWindowPlacement { window, placement },
                cx,
            );
            return;
        }
        let result = match window {
            WorkspaceWindow::Main => self.model.set_main_window(placement),
            WorkspaceWindow::Floating(window) => {
                self.model.set_floating_window_placement(window, placement)
            }
        };
        if result.is_ok() {
            self.publish_document(cx);
        }
    }

    pub fn activate_or_show(
        &mut self,
        view: DocumentViewId,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if let Some(revision) = self.authority_revision() {
            let command = match self.model.document().location(view)? {
                DocumentViewLocation::Hidden => {
                    WorkspaceLayoutCommand::ReopenTab(PaneInstanceId(view))
                }
                DocumentViewLocation::Docked | DocumentViewLocation::Floating(_) => {
                    WorkspaceLayoutCommand::FocusPane(PaneInstanceId(view))
                }
            };
            self.execute_layout_command(revision, command, cx)?;
            self.emit(DynamicWorkspaceUiEvent::Activated(view), cx);
            return Ok(());
        }
        match self.model.document().location(view)? {
            DocumentViewLocation::Docked => {
                let item = self
                    .model
                    .item(view)
                    .ok_or(DynamicWorkspaceUiError::UnknownView(view))?;
                self.panes.update(cx, |panes, cx| {
                    if let Some(pane) = panes.pane_of(item) {
                        panes.activate(pane, item, cx);
                    }
                });
            }
            DocumentViewLocation::Floating(window) => {
                if let Some(record) = self.floating.get(&window) {
                    record
                        .handle
                        .update(cx, |_root, window, _cx| window.activate_window())
                        .ok();
                }
            }
            DocumentViewLocation::Hidden => {
                self.model.show_view(view)?;
                let item = self
                    .model
                    .item(view)
                    .ok_or(DynamicWorkspaceUiError::UnknownView(view))?;
                self.panes
                    .update(cx, |panes, cx| panes.add_to_focused(item, cx));
                self.publish_document(cx);
            }
        }
        Ok(())
    }

    /// Create a target-bearing editor instance and add it to a requested Guise
    /// pane. The factory produces a distinct GPUI entity for every descriptor.
    pub fn create_view(
        &mut self,
        descriptor: NewWorkspaceView,
        destination: Option<PaneId>,
        cx: &mut Context<Self>,
    ) -> Result<DocumentViewId, DynamicWorkspaceUiError> {
        if let Some(revision) = self.authority_revision() {
            if destination.is_some() {
                return Err(DynamicWorkspaceUiError::AuthorityDestinationRequiresDockPane);
            }
            let mut projected = self.model.clone();
            let (view, _) = projected.create_view(descriptor)?;
            let descriptor = projected
                .descriptor(view)
                .cloned()
                .ok_or(DynamicWorkspaceUiError::UnknownView(view))?;
            if let Err(error) = self.registry.ensure(&descriptor, cx) {
                return Err(error);
            }
            projected.show_view(view)?;
            if let Err(error) = self.execute_layout_command(
                revision,
                WorkspaceLayoutCommand::ReplaceDocument {
                    document: projected.export_document(),
                },
                cx,
            ) {
                self.registry.remove(view);
                return Err(error);
            }
            return Ok(view);
        }
        let (view, item) = self.model.create_view(descriptor)?;
        let descriptor = self
            .model
            .descriptor(view)
            .cloned()
            .ok_or(DynamicWorkspaceUiError::UnknownView(view))?;
        if let Err(error) = self.registry.ensure(&descriptor, cx) {
            let _ = self.model.close_view(view);
            return Err(error);
        }
        self.registry.bind_all(self.model.item_map());
        self.model.show_view(view)?;
        self.panes.update(cx, |panes, cx| {
            if let Some(destination) = destination {
                panes.add_item(destination, item, cx);
            } else {
                panes.add_to_focused(item, cx);
            }
        });
        self.publish_document(cx);
        Ok(view)
    }

    pub fn replace_view_descriptor(
        &mut self,
        descriptor: WorkspaceViewDescriptor,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if let Some(revision) = self.authority_revision() {
            let mut projected = self.model.clone();
            projected.replace_view(descriptor.clone())?;
            self.registry.ensure(&descriptor, cx)?;
            self.execute_layout_command(
                revision,
                WorkspaceLayoutCommand::ReplaceDocument {
                    document: projected.export_document(),
                },
                cx,
            )?;
            cx.notify();
            return Ok(());
        }
        self.model.replace_view(descriptor.clone())?;
        self.registry.ensure(&descriptor, cx)?;
        self.publish_document(cx);
        cx.notify();
        Ok(())
    }

    pub fn float_view(
        &mut self,
        view: DocumentViewId,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if let Some(revision) = self.authority_revision() {
            let accepted = self.execute_layout_command(
                revision,
                WorkspaceLayoutCommand::TearOffPane {
                    pane: PaneInstanceId(view),
                    placement: None,
                },
                cx,
            )?;
            let window = accepted
                .transition
                .windows
                .iter()
                .find_map(|effect| match effect {
                    NativeWindowEffect::Open { window, .. } => Some(*window),
                    _ => None,
                })
                .ok_or(DynamicWorkspaceUiError::MissingAcceptedWindowEffect)?;
            self.emit(DynamicWorkspaceUiEvent::Floated { view, window }, cx);
            return Ok(());
        }
        let window = self.model.float_view(view, None)?;
        if let Some(item) = self.model.item(view) {
            self.panes
                .update(cx, |panes, cx| panes.close_item(item, cx));
        }
        self.open_floating_window(window, None, cx);
        self.emit(DynamicWorkspaceUiEvent::Floated { view, window }, cx);
        self.publish_document(cx);
        Ok(())
    }

    pub fn dock_view(
        &mut self,
        view: DocumentViewId,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if let Some(revision) = self.authority_revision() {
            let location = self.model.document().location(view)?;
            if matches!(location, DocumentViewLocation::Docked) {
                return self.activate_or_show(view, cx);
            }
            self.execute_layout_command(
                revision,
                WorkspaceLayoutCommand::MovePane {
                    pane: PaneInstanceId(view),
                    destination: PaneMoveDestination {
                        window: WorkspaceWindow::Main,
                        dock_pane: self.model.document().main_layout.primary_pane(),
                        tab_index: usize::MAX,
                    },
                },
                cx,
            )?;
            self.emit(DynamicWorkspaceUiEvent::Docked(view), cx);
            return Ok(());
        }
        let old_window = match self.model.document().location(view)? {
            DocumentViewLocation::Floating(window) => Some(window),
            _ => None,
        };
        self.model.dock_view(view)?;
        let item = self
            .model
            .item(view)
            .ok_or(DynamicWorkspaceUiError::UnknownView(view))?;
        self.panes
            .update(cx, |panes, cx| panes.add_to_focused(item, cx));
        if let Some(window) = old_window {
            if !self.model.document().floating_windows.contains_key(&window) {
                self.close_native_window(window, cx);
            }
        }
        self.emit(DynamicWorkspaceUiEvent::Docked(view), cx);
        self.publish_document(cx);
        Ok(())
    }

    pub fn close_view(
        &mut self,
        view: DocumentViewId,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if let Some(revision) = self.authority_revision() {
            self.execute_layout_command(
                revision,
                WorkspaceLayoutCommand::CloseTab(PaneInstanceId(view)),
                cx,
            )?;
            self.emit(DynamicWorkspaceUiEvent::Closed(view), cx);
            return Ok(());
        }
        let old_window = match self.model.document().location(view)? {
            DocumentViewLocation::Floating(window) => Some(window),
            _ => None,
        };
        let item = self.model.item(view);
        self.model.close_view(view)?;
        if let Some(item) = item {
            self.panes
                .update(cx, |panes, cx| panes.close_item(item, cx));
        }
        let removed = !self.model.document().views.contains_key(&view);
        if removed {
            self.registry.remove(view);
        }
        if let Some(window) = old_window {
            if !self.model.document().floating_windows.contains_key(&window) {
                self.close_native_window(window, cx);
            }
        }
        self.emit(
            if removed {
                DynamicWorkspaceUiEvent::Removed(view)
            } else {
                DynamicWorkspaceUiEvent::Closed(view)
            },
            cx,
        );
        self.publish_document(cx);
        Ok(())
    }

    fn handle_group_event(
        &mut self,
        source_window: Option<DocumentWindowId>,
        panes: Entity<PaneGroup>,
        event: &PaneGroupEvent,
        cx: &mut Context<Self>,
    ) {
        if self.actuating_authority {
            return;
        }
        match event {
            PaneGroupEvent::Activated(item) => {
                if let Some(view) = self.model.view(*item) {
                    if let Some(revision) = self.authority_revision() {
                        let window = source_window
                            .map(WorkspaceWindow::Floating)
                            .unwrap_or(WorkspaceWindow::Main);
                        let needs_command = self.authority.as_ref().is_some_and(|authority| {
                            pane_activation_needs_authority_command(
                                authority.layout(),
                                window,
                                view,
                            )
                        });
                        if needs_command {
                            // `Activated` describes a Guise mutation that has
                            // already happened. Restoring the old portable
                            // document here would enqueue an activation for
                            // the old tab; applying FocusPane would then
                            // enqueue one for the new tab, and the two deferred
                            // events could oscillate forever. Lower the native
                            // event directly, and absorb the matching event
                            // emitted by authoritative actuation above.
                            if self
                                .execute_layout_command(
                                    revision,
                                    authority_command_for_pane_intent(
                                        view,
                                        DynamicPaneAuthorityIntent::Activate,
                                    ),
                                    cx,
                                )
                                .is_err()
                            {
                                let _ = self.restore_portable_surface_before_input(cx);
                                return;
                            }
                        }
                    }
                    self.emit(DynamicWorkspaceUiEvent::Activated(view), cx);
                }
            }
            PaneGroupEvent::CloseRequested(item) => {
                let Some(view) = self.model.view(*item) else {
                    return;
                };
                if let Some(revision) = self.authority_revision() {
                    if self.restore_portable_surface_before_input(cx).is_err() {
                        return;
                    }
                    match self.execute_layout_command(
                        revision,
                        authority_command_for_pane_intent(view, DynamicPaneAuthorityIntent::Close),
                        cx,
                    ) {
                        Ok(_) => self.emit(DynamicWorkspaceUiEvent::Closed(view), cx),
                        Err(error) => self.emit(
                            DynamicWorkspaceUiEvent::CloseDenied {
                                view,
                                message: error.to_string().into(),
                            },
                            cx,
                        ),
                    }
                    return;
                }
                let window = match self.model.document().location(view) {
                    Ok(DocumentViewLocation::Floating(window)) => Some(window),
                    _ => None,
                };
                match self.model.close_view(view) {
                    Ok(()) => {
                        panes.update(cx, |panes, cx| panes.close_item(*item, cx));
                        let removed = !self.model.document().views.contains_key(&view);
                        if removed {
                            self.registry.remove(view);
                        }
                        if let Some(window) = window {
                            if !self.model.document().floating_windows.contains_key(&window) {
                                self.close_native_window(window, cx);
                            }
                        }
                        self.emit(
                            if removed {
                                DynamicWorkspaceUiEvent::Removed(view)
                            } else {
                                DynamicWorkspaceUiEvent::Closed(view)
                            },
                            cx,
                        );
                        self.publish_document(cx);
                    }
                    Err(error) => self.emit(
                        DynamicWorkspaceUiEvent::CloseDenied {
                            view,
                            message: error.to_string().into(),
                        },
                        cx,
                    ),
                }
            }
            PaneGroupEvent::NewRequested(pane) => self.emit(
                DynamicWorkspaceUiEvent::NewViewRequested {
                    window: source_window,
                    pane: *pane,
                },
                cx,
            ),
            PaneGroupEvent::FocusChanged(pane) => {
                let active = panes
                    .read(cx)
                    .panes_with_items()
                    .into_iter()
                    .find_map(|(candidate, _, active)| (candidate == *pane).then_some(active));
                let Some(view) = active.and_then(|item| self.model.view(item)) else {
                    return;
                };
                if let Some(revision) = self.authority_revision() {
                    // Guise focus is window-local and not part of its snapshot.
                    // Commit the active stable view so focus survives native
                    // window activation and document round trips. As with an
                    // Activated event, this is already-applied native input;
                    // do not restore the old surface before lowering it, and
                    // absorb the FocusChanged echo from native actuation.
                    let window = source_window
                        .map(WorkspaceWindow::Floating)
                        .unwrap_or(WorkspaceWindow::Main);
                    let needs_command = self.authority.as_ref().is_some_and(|authority| {
                        pane_activation_needs_authority_command(authority.layout(), window, view)
                    });
                    if needs_command
                        && self
                            .execute_layout_command(
                                revision,
                                WorkspaceLayoutCommand::FocusPane(PaneInstanceId(view)),
                                cx,
                            )
                            .is_err()
                    {
                        let _ = self.restore_portable_surface_before_input(cx);
                        return;
                    }
                }
                self.emit(DynamicWorkspaceUiEvent::Activated(view), cx);
            }
            PaneGroupEvent::TearOff(item) => {
                let Some(view) = self.model.view(*item) else {
                    return;
                };
                if let Some(revision) = self.authority_revision() {
                    if self.restore_portable_surface_before_input(cx).is_err() {
                        return;
                    }
                    match self.execute_layout_command(
                        revision,
                        authority_command_for_pane_intent(
                            view,
                            DynamicPaneAuthorityIntent::TearOff,
                        ),
                        cx,
                    ) {
                        Ok(accepted) => {
                            if let Some(window) =
                                accepted
                                    .transition
                                    .windows
                                    .iter()
                                    .find_map(|effect| match effect {
                                        NativeWindowEffect::Open { window, .. } => Some(*window),
                                        _ => None,
                                    })
                            {
                                self.emit(DynamicWorkspaceUiEvent::Floated { view, window }, cx);
                            }
                        }
                        Err(error) => self.emit(
                            DynamicWorkspaceUiEvent::WindowOpenFailed {
                                view,
                                message: error.to_string().into(),
                            },
                            cx,
                        ),
                    }
                    return;
                }
                let old_window = source_window;
                match self.model.tear_off_view(view, None) {
                    Ok(window) => {
                        if let Some(old_window) = old_window {
                            if !self
                                .model
                                .document()
                                .floating_windows
                                .contains_key(&old_window)
                            {
                                self.close_native_window(old_window, cx);
                            }
                        }
                        self.open_floating_window(window, None, cx);
                        self.emit(DynamicWorkspaceUiEvent::Floated { view, window }, cx);
                        self.publish_document(cx);
                    }
                    Err(error) => {
                        panes.update(cx, |panes, cx| panes.add_to_focused(*item, cx));
                        self.emit(
                            DynamicWorkspaceUiEvent::WindowOpenFailed {
                                view,
                                message: error.to_string().into(),
                            },
                            cx,
                        );
                    }
                }
            }
            PaneGroupEvent::ContextMenu { item, position } => {
                if let Some(view) = self.model.view(*item) {
                    self.emit(
                        DynamicWorkspaceUiEvent::ContextMenuRequested {
                            view,
                            position: *position,
                        },
                        cx,
                    );
                }
            }
        }
    }

    fn open_floating_window(
        &mut self,
        window_id: DocumentWindowId,
        placement: Option<WindowPlacement>,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.try_open_floating_window(window_id, placement, cx) {
            let views = self
                .model
                .document()
                .floating_windows
                .get(&window_id)
                .map(|floating| document_layout_views(&floating.layout))
                .unwrap_or_default();
            let _ = self.model.dock_window(window_id);
            for view in views {
                if let Some(item) = self.model.item(view) {
                    self.panes
                        .update(cx, |panes, cx| panes.add_to_focused(item, cx));
                }
                self.emit(
                    DynamicWorkspaceUiEvent::WindowOpenFailed {
                        view,
                        message: error.to_string().into(),
                    },
                    cx,
                );
            }
        }
    }

    fn restore_floating_window(
        &mut self,
        window_id: DocumentWindowId,
        placement: Option<WindowPlacement>,
        cx: &mut Context<Self>,
    ) {
        if self.authority.is_none() {
            self.open_floating_window(window_id, placement, cx);
            return;
        }
        if let Err(error) = self.try_open_floating_window(window_id, placement, cx) {
            let views = self
                .model
                .document()
                .floating_windows
                .get(&window_id)
                .map(|floating| document_layout_views(&floating.layout))
                .unwrap_or_default();
            if let Some(revision) = self.authority_revision() {
                let _ = self.execute_layout_command(
                    revision,
                    WorkspaceLayoutCommand::DockWindow(window_id),
                    cx,
                );
            }
            for view in views {
                self.emit(
                    DynamicWorkspaceUiEvent::WindowOpenFailed {
                        view,
                        message: error.to_string().into(),
                    },
                    cx,
                );
            }
        }
    }

    fn try_open_floating_window(
        &mut self,
        window_id: DocumentWindowId,
        placement: Option<WindowPlacement>,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
        if self.floating.contains_key(&window_id) {
            return Ok(());
        }
        let layout = self.model.floating_guise_layout(window_id)?;
        let registry = self.registry.clone();
        let workspace = cx.weak_entity();
        let options = dynamic_floating_options(placement, cx);
        let panes_slot = Rc::new(RefCell::new(None));
        let opened_panes = Rc::clone(&panes_slot);
        let result = cx.open_window(options, move |window, cx| {
            let floating_workspace = workspace.clone();
            let root = cx.new(|cx| {
                DynamicFloatingWindow::new(
                    window_id,
                    layout,
                    registry,
                    floating_workspace,
                    window,
                    cx,
                )
            });
            opened_panes
                .borrow_mut()
                .replace(root.read(cx).panes.clone());
            window.focus(&root.focus_handle(cx));
            root
        });
        match result {
            Ok(handle) => {
                let panes = panes_slot.borrow_mut().take().ok_or(
                    DynamicWorkspaceUiError::NativeWindow {
                        operation: "open_window",
                        message: "floating pane root was not created".into(),
                    },
                )?;
                self.floating.insert(
                    window_id,
                    DynamicFloatingRecord {
                        handle: handle.into(),
                        panes,
                    },
                );
                Ok(())
            }
            Err(error) => Err(DynamicWorkspaceUiError::NativeWindow {
                operation: "open_window",
                message: error.to_string().into(),
            }),
        }
    }

    fn native_window_closed(&mut self, window: DocumentWindowId, cx: &mut Context<Self>) {
        self.floating.remove(&window);
        if self.shutting_down || !self.model.document().floating_windows.contains_key(&window) {
            return;
        }
        let views = self
            .model
            .document()
            .floating_windows
            .get(&window)
            .map(|floating| document_layout_views(&floating.layout))
            .unwrap_or_default();
        if let Some(revision) = self.authority_revision() {
            if self
                .execute_layout_command(revision, WorkspaceLayoutCommand::DockWindow(window), cx)
                .is_ok()
            {
                for view in views {
                    self.emit(DynamicWorkspaceUiEvent::Docked(view), cx);
                }
            }
            return;
        }
        if self.model.dock_window(window).is_ok() {
            for view in views {
                if let Some(item) = self.model.item(view) {
                    self.panes
                        .update(cx, |panes, cx| panes.add_to_focused(item, cx));
                }
                self.emit(DynamicWorkspaceUiEvent::Docked(view), cx);
            }
            self.publish_document(cx);
        }
    }

    fn close_native_window(&mut self, window: DocumentWindowId, cx: &mut Context<Self>) {
        if let Some(record) = self.floating.remove(&window) {
            cx.defer(move |cx| {
                let _ = record
                    .handle
                    .update(cx, |_root, window, _cx| window.remove_window());
            });
        }
    }

    fn sync_layout(
        &mut self,
        window: Option<DocumentWindowId>,
        snapshot: &guise::panegroup::LayoutSnapshot,
        cx: &mut Context<Self>,
    ) {
        if self.actuating_authority {
            return;
        }
        if let Some(revision) = self.authority_revision() {
            let mut projected = self.model.clone();
            let translated = match window {
                Some(window) => projected
                    .replace_floating_layout(window, snapshot)
                    .map(|_| {
                        projected.document().floating_windows[&window]
                            .layout
                            .clone()
                    }),
                None => projected
                    .replace_main_layout(snapshot)
                    .map(|_| projected.document().main_layout.clone()),
            };
            match translated {
                Ok(layout) => {
                    let target = window
                        .map(WorkspaceWindow::Floating)
                        .unwrap_or(WorkspaceWindow::Main);
                    let current = match target {
                        WorkspaceWindow::Main => &self.model.document().main_layout,
                        WorkspaceWindow::Floating(window) => {
                            &self.model.document().floating_windows[&window].layout
                        }
                    };
                    if *current == layout {
                        return;
                    }
                    // Guise only exposes the completed split/tab snapshot. Put
                    // the transient native mutation back to current portable
                    // truth, then accept and actuate the proposal normally so
                    // the lasting layout change is still document-first.
                    self.actuating_authority = true;
                    let restore_result = match target {
                        WorkspaceWindow::Main => {
                            let snapshot = self.model.main_guise_layout();
                            snapshot.map(|snapshot| {
                                self.panes
                                    .update(cx, |panes, cx| panes.restore(&snapshot, cx))
                            })
                        }
                        WorkspaceWindow::Floating(window) => {
                            let snapshot = self.model.floating_guise_layout(window);
                            snapshot.map(|snapshot| {
                                self.floating.get(&window).is_some_and(|record| {
                                    record
                                        .panes
                                        .update(cx, |panes, cx| panes.restore(&snapshot, cx))
                                })
                            })
                        }
                    };
                    self.actuating_authority = false;
                    if !matches!(restore_result, Ok(true)) {
                        self.emit(
                            DynamicWorkspaceUiEvent::NativeActuationFailed {
                                operation: "restore_before_layout_command",
                                message: "could not restore portable layout before accepting the native proposal".into(),
                                recovery_diagnostics: Vec::new(),
                            },
                            cx,
                        );
                        return;
                    }
                    if self
                        .execute_layout_command(
                            revision,
                            WorkspaceLayoutCommand::ReplaceWindowLayout {
                                window: target,
                                layout,
                            },
                            cx,
                        )
                        .is_ok()
                    {
                        self.emit(DynamicWorkspaceUiEvent::LayoutChanged { window }, cx);
                    }
                }
                Err(error) => self.emit(
                    DynamicWorkspaceUiEvent::NativeActuationFailed {
                        operation: "translate_layout",
                        message: error.to_string().into(),
                        recovery_diagnostics: Vec::new(),
                    },
                    cx,
                ),
            }
            return;
        }
        let result = match window {
            Some(window) => self.model.replace_floating_layout(window, snapshot),
            None => self.model.replace_main_layout(snapshot),
        };
        if result.is_ok() {
            self.emit(DynamicWorkspaceUiEvent::LayoutChanged { window }, cx);
            self.publish_document(cx);
        }
    }

    fn emit(&self, event: DynamicWorkspaceUiEvent, cx: &mut Context<Self>) {
        (self.hooks.on_event)(event.clone(), cx);
        cx.emit(event);
    }

    fn publish_document(&self, cx: &mut App) {
        (self.hooks.on_snapshot)(self.model.export_document(), cx);
    }
}

impl Render for DynamicWorkspaceRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().size_full().flex().flex_col();
        if let Some(chrome) = &self.chrome {
            root = root.child((chrome)(window, cx));
        }
        root.child(div().flex_1().min_h_0().child(self.panes.clone()))
    }
}

impl gpui::EventEmitter<DynamicWorkspaceUiEvent> for DynamicWorkspaceRoot {}

struct DynamicFloatingWindow {
    window_id: DocumentWindowId,
    panes: Entity<PaneGroup>,
    workspace: WeakEntity<DynamicWorkspaceRoot>,
    focus: FocusHandle,
}

impl DynamicFloatingWindow {
    fn new(
        window_id: DocumentWindowId,
        layout: guise::panegroup::LayoutSnapshot,
        registry: DynamicPaneRegistry,
        workspace: WeakEntity<DynamicWorkspaceRoot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let panes = create_dynamic_group(&layout, &registry, cx)
            .expect("validated floating document has at least one registered item");

        let event_workspace = workspace.clone();
        cx.subscribe(&panes, move |_this, panes, event, cx| {
            event_workspace
                .update(cx, |root, cx| {
                    root.handle_group_event(Some(window_id), panes, event, cx)
                })
                .ok();
        })
        .detach();
        let layout_workspace = workspace.clone();
        cx.observe(&panes, move |_this, panes, cx| {
            let snapshot = panes.read(cx).snapshot();
            layout_workspace
                .update(cx, |root, cx| {
                    root.sync_layout(Some(window_id), &snapshot, cx)
                })
                .ok();
        })
        .detach();
        let bounds_workspace = workspace.clone();
        cx.observe_window_bounds(window, move |_this, window, cx| {
            let placement = document_placement_from_gpui(window.window_bounds());
            bounds_workspace
                .update(cx, |root, cx| {
                    if !root.actuating_authority {
                        root.record_window_placement(
                            WorkspaceWindow::Floating(window_id),
                            Some(placement),
                            cx,
                        );
                    }
                })
                .ok();
        })
        .detach();
        let close_workspace = workspace.clone();
        window.on_window_should_close(cx, move |_window, cx| {
            close_workspace
                .update(cx, |root, cx| root.native_window_closed(window_id, cx))
                .ok();
            true
        });

        Self {
            window_id,
            panes,
            workspace,
            focus: cx.focus_handle(),
        }
    }
}

impl Focusable for DynamicFloatingWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DynamicFloatingWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let _keep_workspace_alive = &self.workspace;
        div()
            .id(("audec-floating-workspace", self.window_id.0 as usize))
            .size_full()
            .track_focus(&self.focus)
            .child(self.panes.clone())
    }
}

fn create_dynamic_group<Host>(
    layout: &guise::panegroup::LayoutSnapshot,
    registry: &DynamicPaneRegistry,
    cx: &mut Context<Host>,
) -> Result<Entity<PaneGroup>, DynamicWorkspaceUiError>
where
    Host: 'static,
{
    let first_raw = layout
        .item_ids()
        .into_iter()
        .next()
        .ok_or(DynamicWorkspaceUiError::EmptyLayout)?;
    let first = registry
        .raw_items
        .borrow()
        .get(&first_raw)
        .copied()
        .ok_or(DynamicWorkspaceUiError::UnknownRuntimeItem(first_raw))?;

    // `restore` reconstructs ItemIds from the raw ordinals in the snapshot;
    // the first handle only seeds PaneGroup before that atomic replacement.
    let render_registry = registry.clone();
    let title_registry = registry.clone();
    let dot_registry = registry.clone();
    let group = cx.new(|cx| {
        let group = PaneGroup::new(first, cx).tab_height(30.0);
        // Guise's titlebar mode reserves this leading strip only on the
        // top-left pane, so split layouts keep their full width while the
        // first tab stays clear of the macOS traffic lights. It also turns
        // the unused top-row strip into a native window drag region.
        #[cfg(target_os = "macos")]
        let group = {
            let safe = default_workspace_titlebar_layout(
                WindowPlatform::MacOs,
                TitlebarComposition::OverlayTabs,
                // GPUI 0.2 does not expose traffic-light geometry. The
                // shared policy owns the logical-pixel fallback and clearance.
                None,
            )
            .expect("static titlebar metrics are valid");
            let (leading, trailing) = safe.guise_titlebar_insets();
            group.titlebar(leading, trailing)
        };

        group
            .on_render_item(move |item, window, cx| {
                render_registry
                    .pane_for_item(item)
                    .map(|pane| pane.element(window, cx))
                    .unwrap_or_else(|| missing_pane(render_registry.missing_message_for_item(item)))
            })
            .on_item_title(move |item, _cx| title_registry.title_for_item(item))
            .on_item_dot(move |item, cx| {
                dot_registry
                    .pane_for_item(item)
                    .and_then(|pane| (pane.dot)(cx))
            })
    });
    group.update(cx, |group, cx| {
        let restored = group.restore(layout, cx);
        debug_assert!(restored, "validated dynamic workspace must restore");
    });
    Ok(group)
}

fn document_layout_views(layout: &DockLayout) -> Vec<DocumentViewId> {
    fn collect(layout: &DockLayout, out: &mut Vec<DocumentViewId>) {
        match layout {
            DockLayout::Pane { items, .. } => out.extend(items),
            DockLayout::Split { first, second, .. } => {
                collect(first, out);
                collect(second, out);
            }
        }
    }
    let mut views = Vec::new();
    collect(layout, &mut views);
    views
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DynamicPaneAuthorityIntent {
    Activate,
    Close,
    TearOff,
}

fn authority_command_for_pane_intent(
    view: DocumentViewId,
    intent: DynamicPaneAuthorityIntent,
) -> WorkspaceLayoutCommand {
    match intent {
        DynamicPaneAuthorityIntent::Activate => {
            WorkspaceLayoutCommand::FocusPane(PaneInstanceId(view))
        }
        DynamicPaneAuthorityIntent::Close => WorkspaceLayoutCommand::CloseTab(PaneInstanceId(view)),
        DynamicPaneAuthorityIntent::TearOff => WorkspaceLayoutCommand::TearOffPane {
            pane: PaneInstanceId(view),
            placement: None,
        },
    }
}

fn pane_activation_needs_authority_command(
    layout: &crate::workspace_session_layout::WorkspaceSessionLayout,
    window: WorkspaceWindow,
    view: DocumentViewId,
) -> bool {
    let pane = PaneInstanceId(view);
    // A source-window disagreement means this cannot be an acknowledgement of
    // the authoritative focus effect. Let the command path diagnose it rather
    // than swallowing a real (or corrupt) cross-window transition.
    layout.placement(pane).is_none_or(|placement| {
        placement.window != window || layout.focused_pane(window) != Some(pane)
    })
}

fn dynamic_floating_options(placement: Option<WindowPlacement>, cx: &mut App) -> WindowOptions {
    let bounds = placement
        .and_then(|placement| document_placement_to_gpui(placement).ok())
        .unwrap_or_else(|| {
            WindowBounds::Windowed(Bounds::centered(None, size(px(1_080.0), px(720.0)), cx))
        });
    WindowOptions {
        window_bounds: Some(bounds),
        window_min_size: Some(size(px(560.0), px(360.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("audec — workspace".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[derive(Clone, Debug)]
pub enum DynamicWorkspaceUiError {
    Model(DynamicWorkspaceError),
    Document(crate::workspace_document::WorkspaceDocumentError),
    Authority(WorkspaceAuthorityError),
    Semantic(WorkspaceSemanticError),
    PortableAuthorityNotInstalled,
    NativeLayoutRejected {
        window: Option<DocumentWindowId>,
    },
    MissingNativeWindow(DocumentWindowId),
    MissingAcceptedWindowEffect,
    AuthorityDestinationRequiresDockPane,
    NativeWindow {
        operation: &'static str,
        message: SharedString,
    },
    NativeActuation {
        failure: WorkspaceNativeFailure,
        recovery_diagnostics: Vec<String>,
    },
    MissingFactory(DocumentViewId),
    FactoryFailed {
        view: DocumentViewId,
        message: SharedString,
    },
    UnknownView(DocumentViewId),
    UnknownRuntimeItem(u64),
    EmptyLayout,
}

impl std::fmt::Display for DynamicWorkspaceUiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(error) => error.fmt(formatter),
            Self::Document(error) => error.fmt(formatter),
            Self::Authority(error) => error.fmt(formatter),
            Self::Semantic(error) => error.fmt(formatter),
            Self::PortableAuthorityNotInstalled => {
                formatter.write_str("portable workspace session authority is not installed")
            }
            Self::NativeLayoutRejected { window } => match window {
                Some(window) => write!(
                    formatter,
                    "native floating window {} rejected the authoritative layout",
                    window.0
                ),
                None => formatter.write_str("native main window rejected the authoritative layout"),
            },
            Self::MissingNativeWindow(window) => {
                write!(
                    formatter,
                    "native workspace window {} is not open",
                    window.0
                )
            }
            Self::MissingAcceptedWindowEffect => {
                formatter.write_str("accepted workspace command omitted its native window effect")
            }
            Self::AuthorityDestinationRequiresDockPane => formatter
                .write_str("authoritative view creation requires a durable DockPaneId destination"),
            Self::NativeWindow { operation, message } => {
                write!(formatter, "native workspace {operation}: {message}")
            }
            Self::NativeActuation {
                failure,
                recovery_diagnostics,
            } => {
                write!(
                    formatter,
                    "workspace native {} failed: {}",
                    failure.operation, failure.message
                )?;
                if !recovery_diagnostics.is_empty() {
                    write!(
                        formatter,
                        " (rollback diagnostics: {})",
                        recovery_diagnostics.join("; ")
                    )?;
                }
                Ok(())
            }
            Self::MissingFactory(view) => {
                write!(formatter, "workspace view {} has no editor factory", view.0)
            }
            Self::FactoryFailed { view, message } => {
                write!(
                    formatter,
                    "workspace view {} failed to open: {message}",
                    view.0
                )
            }
            Self::UnknownView(view) => write!(formatter, "workspace view {} is unknown", view.0),
            Self::UnknownRuntimeItem(item) => write!(formatter, "Guise item {item} is unknown"),
            Self::EmptyLayout => formatter.write_str("workspace layout is empty"),
        }
    }
}

impl std::error::Error for DynamicWorkspaceUiError {}

impl From<DynamicWorkspaceError> for DynamicWorkspaceUiError {
    fn from(error: DynamicWorkspaceError) -> Self {
        Self::Model(error)
    }
}

impl From<crate::workspace_document::WorkspaceDocumentError> for DynamicWorkspaceUiError {
    fn from(error: crate::workspace_document::WorkspaceDocumentError) -> Self {
        Self::Document(error)
    }
}

impl From<WorkspaceAuthorityError> for DynamicWorkspaceUiError {
    fn from(error: WorkspaceAuthorityError) -> Self {
        Self::Authority(error)
    }
}

impl From<WorkspaceSemanticError> for DynamicWorkspaceUiError {
    fn from(error: WorkspaceSemanticError) -> Self {
        Self::Semantic(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_is_always_pinned() {
        assert_eq!(close_policy(BuiltinView::Track, 6), ClosePolicy::DenyPinned);
        assert!(!can_tear_off(BuiltinView::Track, 6));
    }

    #[test]
    fn last_docked_item_cannot_be_closed_or_floated() {
        assert_eq!(
            close_policy(BuiltinView::Waterfall, 1),
            ClosePolicy::DenyLastDocked
        );
        assert!(!can_tear_off(BuiltinView::Waterfall, 1));
    }

    #[test]
    fn ordinary_views_can_close_and_float_from_nontrivial_workspace() {
        for view in BuiltinView::ALL.into_iter().skip(1) {
            assert_eq!(close_policy(view, 2), ClosePolicy::Allow);
            assert!(can_tear_off(view, 2));
        }
    }

    #[test]
    fn empty_registry_reports_every_stable_builtin() {
        assert_eq!(PaneRegistry::new().missing_builtins(), BuiltinView::ALL);
    }

    #[test]
    fn tracked_pane_overflow_round_trips_modeled_scroll_offsets() {
        let pane = PaneRegistration::renderer("Inspector", |_window, _cx| div().into_any_element())
            .with_overflow(WorkspaceOverflow::Vertical);
        pane.restore_scroll_state(PaneScrollState {
            horizontal: 0.0,
            vertical: 246.5,
        });
        assert_eq!(
            pane.scroll_state(),
            Some(PaneScrollState {
                horizontal: 0.0,
                vertical: 246.5,
            })
        );
    }

    #[test]
    fn unavailable_restored_view_retains_identity_title_and_diagnostic() {
        let model = DynamicWorkspaceModel::new(WorkspaceDocument::default()).unwrap();
        let registry = DynamicPaneRegistry::new();
        registry.bind_all(model.item_map());
        let view = DocumentViewId::RHYTHM;
        let mut descriptor = model.descriptor(view).unwrap().clone();
        descriptor.title_override = Some("Restored rhythm tools".into());
        registry.descriptors.borrow_mut().insert(view, descriptor);
        registry
            .missing
            .borrow_mut()
            .insert(view, "extension is not installed".into());
        let item = model.item(view).unwrap();
        assert_eq!(registry.title_for_item(item), "Restored rhythm tools");
        assert_eq!(
            registry.missing_message_for_item(item),
            "extension is not installed"
        );
        assert_eq!(
            registry.missing_view_diagnostics(),
            vec![MissingWorkspaceViewDiagnostic {
                view,
                message: "extension is not installed".into(),
            }]
        );
    }

    #[test]
    fn dynamic_pane_events_lower_to_portable_commands_before_actuation() {
        let view = DocumentViewId::WATERFALL;
        assert_eq!(
            authority_command_for_pane_intent(view, DynamicPaneAuthorityIntent::Activate),
            WorkspaceLayoutCommand::FocusPane(PaneInstanceId(view))
        );
        assert_eq!(
            authority_command_for_pane_intent(view, DynamicPaneAuthorityIntent::Close),
            WorkspaceLayoutCommand::CloseTab(PaneInstanceId(view))
        );
        assert_eq!(
            authority_command_for_pane_intent(view, DynamicPaneAuthorityIntent::TearOff),
            WorkspaceLayoutCommand::TearOffPane {
                pane: PaneInstanceId(view),
                placement: None,
            }
        );
    }

    #[test]
    fn authoritative_activation_and_focus_echoes_are_idempotent() {
        let mut layout = crate::workspace_session_layout::WorkspaceSessionLayout::from_document(
            crate::project_session::ProjectSessionId(9),
            WorkspaceDocument::default(),
        )
        .unwrap();
        let target = DocumentViewId::SEPARATION;

        assert!(pane_activation_needs_authority_command(
            &layout,
            WorkspaceWindow::Main,
            target,
        ));

        layout.focus_pane(PaneInstanceId(target)).unwrap();

        // Guise emits Activated and FocusChanged when the accepted FocusPane
        // command is projected back into the native PaneGroup. Both handlers
        // use this guard, so those acknowledgements must not become another
        // command/revision and another pair of native focus events.
        assert!(!pane_activation_needs_authority_command(
            &layout,
            WorkspaceWindow::Main,
            target,
        ));
        assert!(pane_activation_needs_authority_command(
            &layout,
            WorkspaceWindow::Main,
            DocumentViewId::COMPONENTS,
        ));
        assert!(pane_activation_needs_authority_command(
            &layout,
            WorkspaceWindow::Floating(DocumentWindowId(99)),
            target,
        ));
    }
}
