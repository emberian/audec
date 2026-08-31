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
    FocusHandle, Focusable, Hsla, Render, SharedString, WeakEntity, Window, WindowBounds,
    WindowOptions,
};
use guise::panegroup::{ItemId, PaneId};
use guise::{Button, PaneGroup, PaneGroupEvent, SplitDirection};

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

type PaneRenderer = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type DotRenderer = Rc<dyn Fn(&App) -> Option<Hsla>>;
type ChromeRenderer = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type SnapshotCallback = Rc<dyn Fn(WorkspaceSnapshotDto, &mut App)>;
type EventCallback = Rc<dyn Fn(WorkspaceUiEvent, &mut App)>;
type FloatingOptions =
    Rc<dyn Fn(BuiltinView, Option<WindowPlacementDto>, &mut App) -> WindowOptions>;

/// A registered workspace pane. Cloning this value clones the renderer's
/// captured `Entity<T>` handle, never the entity state itself.
#[derive(Clone)]
pub struct PaneRegistration {
    title: SharedString,
    render: PaneRenderer,
    dot: DotRenderer,
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

    fn element(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        (self.render)(window, cx)
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
            PaneGroup::new(first, cx)
                .tab_height(30.0)
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
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .child(
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

fn missing_pane(message: &'static str) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(message)
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
                Some(retained) if retained == *descriptor => return Ok(()),
                // Legacy-six entities are installed before their migrated v2
                // descriptors exist. Adopt that first descriptor without
                // recreating a pane which is already live.
                None => {
                    self.descriptors
                        .borrow_mut()
                        .insert(descriptor.id, descriptor.clone());
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
        Ok(())
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
}

type DynamicSnapshotCallback = Rc<dyn Fn(WorkspaceDocument, &mut App)>;
type DynamicEventCallback = Rc<dyn Fn(DynamicWorkspaceUiEvent, &mut App)>;
type ProjectWindowCloseCallback = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

#[derive(Clone)]
pub struct DynamicWorkspaceHooks {
    on_snapshot: DynamicSnapshotCallback,
    on_event: DynamicEventCallback,
    on_project_window_close: ProjectWindowCloseCallback,
}

impl Default for DynamicWorkspaceHooks {
    fn default() -> Self {
        Self {
            on_snapshot: Rc::new(|_, _| {}),
            on_event: Rc::new(|_, _| {}),
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

    pub fn on_project_window_close(
        mut self,
        callback: impl Fn(&mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_project_window_close = Rc::new(callback);
        self
    }
}

#[derive(Clone, Copy)]
struct DynamicFloatingRecord {
    handle: AnyWindowHandle,
}

/// One-shot app-shell handoff. The current `ui::create_workspace` can build
/// its six existing entities and legacy `WorkspaceModel` exactly as before,
/// then pass them here. Subsequent panes should be created by `with_factory`
/// from target-bearing v2 descriptors.
pub struct DynamicWorkspaceBootstrap {
    model: DynamicWorkspaceModel,
    registry: DynamicPaneRegistry,
}

impl DynamicWorkspaceBootstrap {
    pub fn from_legacy_six(
        model: WorkspaceModel,
        panes: PaneRegistry,
    ) -> Result<Self, DynamicWorkspaceUiError> {
        let model = DynamicWorkspaceModel::from_legacy_snapshot(model.snapshot())?;
        let registry = DynamicPaneRegistry::new();
        registry.register_legacy_six(&panes);
        Ok(Self { model, registry })
    }

    pub fn from_document(document: WorkspaceDocument) -> Result<Self, DynamicWorkspaceUiError> {
        Ok(Self {
            model: DynamicWorkspaceModel::new(document)?,
            registry: DynamicPaneRegistry::new(),
        })
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
        DynamicWorkspaceRoot::new(self.model, self.registry, chrome, hooks, window, cx)
    }
}

/// Main root for arbitrary editor/lens instances. It owns one dynamic
/// document adapter and presents its main tree plus every native floating
/// tree. Pane registrations keep the same editor entity alive while the view
/// moves between windows.
pub struct DynamicWorkspaceRoot {
    model: DynamicWorkspaceModel,
    registry: DynamicPaneRegistry,
    panes: Entity<PaneGroup>,
    floating: BTreeMap<DocumentWindowId, DynamicFloatingRecord>,
    chrome: Option<ChromeRenderer>,
    hooks: DynamicWorkspaceHooks,
    shutting_down: bool,
}

impl DynamicWorkspaceRoot {
    pub fn new(
        model: DynamicWorkspaceModel,
        registry: DynamicPaneRegistry,
        chrome: Option<impl Fn(&mut Window, &mut App) -> AnyElement + 'static>,
        hooks: DynamicWorkspaceHooks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Self, DynamicWorkspaceUiError> {
        registry.bind_all(model.item_map());
        registry.reconcile_document(model.document(), cx)?;

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
            let placement = document_placement_from_gpui(window.window_bounds());
            if this.model.set_main_window(Some(placement)).is_ok() {
                this.publish_document(cx);
            }
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
                    let _ = root.model.set_main_window(Some(placement));
                    root.publish_document(cx);
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
                            root.open_floating_window(window, placement, cx)
                        })
                        .ok();
                }
            });
        }

        Ok(Self {
            model,
            registry,
            panes,
            floating: BTreeMap::new(),
            chrome: chrome.map(|render| Rc::new(render) as ChromeRenderer),
            hooks,
            shutting_down: false,
        })
    }

    pub fn document(&self) -> &WorkspaceDocument {
        self.model.document()
    }

    pub fn export_document(&self) -> WorkspaceDocument {
        self.model.export_document()
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
        let next = DynamicWorkspaceModel::new(document)?;
        self.registry.reconcile_document(next.document(), cx)?;
        self.registry.bind_all(next.item_map());
        let layout = next.main_guise_layout()?;
        let handles = self
            .floating
            .values()
            .map(|record| record.handle)
            .collect::<Vec<_>>();
        self.floating.clear();
        self.model = next;
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
            self.open_floating_window(window, placement, cx);
        }
        self.publish_document(cx);
        cx.notify();
        Ok(())
    }

    pub fn pane_group(&self) -> Entity<PaneGroup> {
        self.panes.clone()
    }

    pub fn activate_or_show(
        &mut self,
        view: DocumentViewId,
        cx: &mut Context<Self>,
    ) -> Result<(), DynamicWorkspaceUiError> {
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

    fn handle_group_event(
        &mut self,
        source_window: Option<DocumentWindowId>,
        panes: Entity<PaneGroup>,
        event: &PaneGroupEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaneGroupEvent::Activated(item) => {
                if let Some(view) = self.model.view(*item) {
                    self.emit(DynamicWorkspaceUiEvent::Activated(view), cx);
                }
            }
            PaneGroupEvent::CloseRequested(item) => {
                let Some(view) = self.model.view(*item) else {
                    return;
                };
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
            PaneGroupEvent::FocusChanged(_) => {}
            PaneGroupEvent::TearOff(item) => {
                let Some(view) = self.model.view(*item) else {
                    return;
                };
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
        if self.floating.contains_key(&window_id) {
            return;
        }
        let Ok(layout) = self.model.floating_guise_layout(window_id) else {
            return;
        };
        let registry = self.registry.clone();
        let workspace = cx.weak_entity();
        let options = dynamic_floating_options(placement, cx);
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
            window.focus(&root.focus_handle(cx));
            root
        });
        match result {
            Ok(handle) => {
                self.floating.insert(
                    window_id,
                    DynamicFloatingRecord {
                        handle: handle.into(),
                    },
                );
            }
            Err(error) => {
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
                    if root
                        .model
                        .set_floating_window_placement(window_id, Some(placement))
                        .is_ok()
                    {
                        root.publish_document(cx);
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
        let group = group.titlebar(80.0, 12.0);

        group
            .on_render_item(move |item, window, cx| {
                render_registry
                    .pane_for_item(item)
                    .map(|pane| pane.element(window, cx))
                    .unwrap_or_else(|| missing_pane("This workspace view is not registered"))
            })
            .on_item_title(move |item, _cx| {
                title_registry
                    .pane_for_item(item)
                    .map(|pane| pane.title)
                    .unwrap_or_else(|| SharedString::from("Missing view"))
            })
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
}
