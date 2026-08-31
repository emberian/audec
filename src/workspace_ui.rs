//! GPUI/Guise host for audec's persistent dock, tab, and native-window workspace.
//!
//! This module is intentionally independent of `ui::Workbench` and its private
//! visualizer types. Callers register already-created GPUI entities (or custom
//! render closures) under the stable [`BuiltinView`] identities, then install
//! [`WorkspaceRoot`] as the main window root. A view is never recreated when it
//! moves between the dock and a native floating window.

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, size, AnyElement, AnyWindowHandle, App, Bounds, Context, Entity,
    FocusHandle, Focusable, Hsla, Render, SharedString, WeakEntity, Window, WindowBounds,
    WindowOptions,
};
use guise::panegroup::{ItemId, PaneId};
use guise::{Button, PaneGroup, PaneGroupEvent, SplitDirection};

use crate::workspace::{
    BuiltinView, FloatingWindowId, ViewLocation, WindowPlacementDto, WorkspaceModel,
    WorkspaceSnapshotDto,
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
