# Dockable workspace integration

This document is the implementation contract for moving audec's current GPUI
views into a persistent, tabbed, dockable workspace without replacing their
native waveform, spectrogram, recurrence, HPSS, or Loom renderers.

The API examples below were compile-checked in an isolated crate against the
published releases `gpui = 0.2.2` and `guise-ui = 1.5.3`.

## Dependency and initialization

Use Guise without its default `webview` feature. audec does not need a native
web view, and disabling it avoids pulling Wry and its platform dependencies.

```toml
guise-ui = { version = "=1.5.3", default-features = false }
```

Guise and audec then use the same crates.io `gpui = "0.2.2"`; no Cargo patch
or git GPUI revision is needed. Install a theme before opening the first
window. Match the existing audec palette so adopting pane chrome does not
visually reset the application:

```rust
use guise::prelude::Theme;

Theme::dark()
    .with_body(gpui::rgb(BACKGROUND))
    .with_surface(gpui::rgb(PANEL))
    .with_surface_hover(gpui::rgb(BORDER))
    .with_text(gpui::rgb(TEXT))
    .with_dimmed(gpui::rgb(MUTED))
    .with_border(gpui::rgb(BORDER))
    .with_primary(gpui::rgb(CYAN))
    .init(cx);
```

In 1.5.3, `Theme` is in `guise::prelude`, but `PaneGroup` and
`PaneGroupEvent` are root exports rather than prelude exports:

```rust
use guise::panegroup::{Direction, DropEdge, ItemId, ItemIds, LayoutSnapshot, PaneId};
use guise::{PaneGroup, PaneGroupEvent, SplitDirection};
```

## Ownership: preserve entities, not rendered elements

The workspace must own each view as a GPUI entity. Guise owns only opaque item
IDs and pane geometry; its `on_render_item` callback clones the corresponding
entity into the active pane each frame.

```text
WorkspaceRoot
├── Entity<Workbench>                 project, analysis, transport, audio
├── Entity<PaneGroup>                 docked split/tab topology
└── HashMap<ItemId, WorkspaceItem>    stable item registry
    ├── Entity<TrackSurface>
    ├── Entity<Visualizer>(Waterfall)
    ├── Entity<Visualizer>(Rhythm)
    ├── Entity<Visualizer>(Components)
    ├── Entity<Visualizer>(Separation)
    └── Entity<Visualizer>(Loom)
```

`WorkspaceRoot` should be the main window root. `Workbench` becomes the shared
project/transport owner instead of the owner of secondary windows. A small
`TrackSurface` entity renders the existing whole-song timeline against the
shared workbench. The existing `Visualizer` remains an entity and can be
inserted directly as a `PaneGroup` item.

This arrangement has no reference cycle: the workspace owns the workbench and
view entities; the views may strongly reference the workbench; the
`PaneGroup` render/title closures capture only `WeakEntity<WorkspaceRoot>`.
Do not put the workspace registry inside `Workbench` while `Visualizer` still
holds `Entity<Workbench>`, because that creates
`Workbench -> Visualizer -> Workbench`.

Moving an item between a pane and a native window moves an entity handle. It
does not call `Visualizer::new` again. Thus these existing fields survive a
tear-off and dock-back:

- time/frequency viewport and follow state;
- recomputed spectral image and spectrum settings;
- HPSS result and in-flight generation counter;
- Loom sketch, edited events, reconstruction, and selection;
- focus handle and the `AudecLens` key context.

The canvas plots and custom GPUI drawing remain untouched. Guise supplies the
tab strips, split handles, drop overlays, and layout model around them.

## Constructing the pane group

Allocate all built-in item IDs in one fixed order on every launch. The
constructor of `ItemId` is private in Guise 1.5.3; `ItemIds::next()` is the
public allocator. Stable allocation order is therefore part of audec's saved
workspace format.

```rust
let mut ids = ItemIds::new();
let track = ids.next();
let waterfall = ids.next();
let rhythm = ids.next();
let components = ids.next();
let separation = ids.next();
let loom = ids.next();

let render_workspace = cx.weak_entity();
let title_workspace = render_workspace.clone();
let panes = cx.new(|cx| {
    PaneGroup::new(track, cx)
        .tab_height(30.0)
        .on_render_item(move |item, _window, cx| {
            render_workspace
                .read_with(cx, |workspace, _| workspace.render_item(item))
                .unwrap_or_else(|_| gpui::div().into_any_element())
        })
        .on_item_title(move |item, cx| {
            title_workspace
                .read_with(cx, |workspace, _| workspace.item_title(item))
                .unwrap_or_else(|_| gpui::SharedString::from("Missing view"))
        })
});
```

`render_item` should only look up and clone an entity:

```rust
enum WorkspaceItem {
    Track(gpui::Entity<TrackSurface>),
    Lens(gpui::Entity<Visualizer>),
}

impl WorkspaceItem {
    fn element(&self) -> gpui::AnyElement {
        match self {
            Self::Track(view) => view.clone().into_any_element(),
            Self::Lens(view) => view.clone().into_any_element(),
        }
    }
}
```

The callback is invoked from `PaneGroup`'s render lease and receives
`&mut App`, not a `Context<WorkspaceRoot>`. Looking up a child entity through a weak
workspace reference is safe. Trying to update the root currently rendering
from this callback is not.

Build a useful default layout with model methods after constructing the
group. For example: track and analysis views tabbed in the main pane, with
Loom and Separation in a lower pane.

```rust
panes.update(cx, |group, cx| {
    let center = group.focused_pane();
    group.add_item(center, waterfall, cx);
    group.add_item(center, rhythm, cx);
    group.add_item(center, components, cx);

    let lower = group.split(
        center,
        SplitDirection::Vertical,
        false,
        loom,
        cx,
    );
    group.add_item(lower, separation, cx);
    group.activate(center, track, cx);
});
```

The project transport/header should remain outside the group at first. It is
global DAW state and should not disappear when a tab changes. After docking is
stable, the material browser and inspector can become their own workspace
items. The final main-window composition is then a thin global transport over
a full-size `PaneGroup`.

## Events and commands

Subscribe from the workspace context and retain or detach the subscription:

```rust
cx.subscribe(&panes, |this, group, event: &PaneGroupEvent, cx| match event {
    PaneGroupEvent::Activated(item) => this.item_activated(*item, cx),
    PaneGroupEvent::CloseRequested(item) => this.close_requested(*item, group, cx),
    PaneGroupEvent::NewRequested(pane) => this.show_view_picker(*pane, cx),
    PaneGroupEvent::FocusChanged(_) => this.schedule_layout_save(cx),
    PaneGroupEvent::TearOff(item) => this.open_floating(*item, cx),
    PaneGroupEvent::ContextMenu { item, position } => {
        this.open_tab_menu(*item, *position, cx)
    }
})
.detach();

// Divider drags and programmatic model changes call `cx.notify()` but do not
// have their own PaneGroupEvent in 1.5.3. Observe the entity for persistence.
cx.observe(&panes, |this, _group, cx| this.schedule_layout_save(cx))
    .detach();
```

Use the model API for menu items and shortcuts:

- activate/show existing view: `pane_of(item)` then `activate(pane, item, cx)`;
- new tab in active pane: `add_to_focused(item, cx)`;
- horizontal/vertical split: `split(pane, axis, first, item, cx)`;
- cycle tabs: `activate_next(cx)` / `activate_prev(cx)`;
- spatial focus: `focus_direction(Direction::Left | Right | Up | Down, cx)`;
- pane zoom: `toggle_zoom(cx)`;
- equal splits: `equalize(cx)`;
- keyboard divider resize: `resize_focused(direction, step, cx)`;
- close: `close_item(item, cx)` only after host policy approves;
- float: `tear_off(item, cx)`; the item is already detached when the event is
  received.

Treat the track surface as pinned. Guise 1.5.3 always draws a close affordance
and will emit `CloseRequested` even for the last item; the host must ignore a
close request for the track and any request that would leave the entire group
empty. `PaneGroup::tear_off` itself refuses to detach the group's last item.

The tab context menu should provide at least Close, Close Other Views, Split
Right, Split Down, Float to New Window, Move to Main Area, Reset Layout, and
Equalize Splits. The explicit Float action is also a reliable fallback for an
outside-window tab drag.

## Native tear-off and dock-back

`PaneGroup::tear_off(item, cx)` performs only the model half: it removes the
item and emits `PaneGroupEvent::TearOff`. Open the native GPUI window from the
workspace event handler with `cx.defer`, because the event originates during
another entity update:

```rust
fn open_floating(&mut self, item: ItemId, cx: &mut Context<Self>) {
    let Some(view) = self.items.get(&item).map(WorkspaceItem::clone_handle) else {
        return;
    };
    let workspace = cx.weak_entity();

    cx.defer(move |cx| {
        let options = floating_window_options(item, cx);
        let floating_workspace = workspace.clone();
        if let Ok(handle) = cx.open_window(options, move |window, cx| {
            let root = cx.new(|cx| {
                FloatingView::new(item, view, floating_workspace, window, cx)
            });
            window.focus(&root.focus_handle(cx));
            root
        }) {
            let any_handle = gpui::AnyWindowHandle::from(handle);
            workspace
                .update(cx, |this, cx| this.did_open_floating(item, any_handle, cx))
                .ok();
        }
    });
}
```

`WindowHandle<T>` does not keep a window alive and is not `Clone` in GPUI
0.2.2. Store the `Copy` `AnyWindowHandle` when the controller needs to find,
activate, or close a floating window later.

`FloatingView` owns a clone of the same view entity plus a weak workspace
reference. Its header has a Dock Back control. Closing the native window also
docks the item, so state is never discarded accidentally:

```rust
window.on_window_should_close(cx, move |_window, cx| {
    workspace
        .update(cx, |this, cx| this.dock_to_main(item, cx))
        .ok();
    true
});
```

Dock-back removes the floating-window record, calls
`main_group.add_to_focused(item, cx)`, and then closes the native window with
`window.remove_window()`. Guard it with an item-location enum so the close hook
and Dock Back button are idempotent.

```rust
enum ItemLocation {
    Main,
    Floating(gpui::AnyWindowHandle),
}
```

Guise 1.5.3 does not provide a controller that moves items between different
`PaneGroup` entities, nor a window target for dock-back. Therefore the robust
1.5.3 interaction is an explicit Dock Back button/menu/shortcut. Dragging and
splitting inside one group are built in. True tab dragging between native OS
windows requires a later app-level drop broker (or an upstream Guise API), and
should not be implied by the initial integration.

## Saved layout and window placement

Guise persists the split tree, per-pane tab order, active tab, and divider
ratios:

```rust
let encoded = panes.read(cx).snapshot().encode();

if let Ok(snapshot) = LayoutSnapshot::decode(&encoded) {
    let known = snapshot
        .item_ids()
        .into_iter()
        .all(|raw| persisted_item_descriptors.contains_key(&raw));
    if known {
        panes.update(cx, |group, cx| {
            let restored = group.restore(&snapshot, cx);
            debug_assert!(restored);
        });
    }
}
```

`restore` rejects empty panes and duplicate item IDs and leaves the old layout
unchanged, but it cannot verify that audec can render an ID. Check
`snapshot.item_ids()` against the host registry first. Ratios are clamped by
Guise on restore.

The Guise snapshot contains only the items currently in that group. audec's
workspace file must wrap it with host-owned state:

```text
version
main PaneGroup snapshot string
stable item-id -> view-kind descriptors
item-id -> Main | Floating(window-key)
main window bounds/state
floating window key -> bounds/state
```

Serialize `gpui::WindowBounds` into a plain representation containing state
(`Windowed`, `Maximized`, or `Fullscreen`) and `x`, `y`, `width`, `height`.
Read current placement from `window.window_bounds()`; observe changes with
`cx.observe_window_bounds(window, ...)`. Reconstruct the corresponding
`WindowBounds` variant in `WindowOptions` on launch.

Write the file atomically (temporary sibling, flush, rename), debounce repeated
divider/window resize events, and version it. A corrupt or incompatible file
must fall back to the default layout without preventing the app from opening.
Keep a `shutting_down` guard so native-window close callbacks do not rewrite a
saved multi-window layout as "everything docked" during application teardown.

## Incremental patch plan

1. **Dependency and theme.** Add Guise 1.5.3 without default features, install
   the audec-colored theme before `open_window`, and confirm the existing app
   renders unchanged.
2. **Entity registry.** Add `WorkspaceRoot`, deterministic built-in item IDs,
   and a `WorkspaceItem` registry. Extract the current whole-song center into
   `TrackSurface`; keep `Workbench` as shared project/audio state.
3. **Dock in place.** Create one `PaneGroup`, render the existing `Visualizer`
   entities through it, wire activation/close/new/context-menu events, and
   replace the old "open a new window" buttons with activate-or-add behavior.
4. **State-preserving float.** Handle `TearOff` with a deferred native GPUI
   window whose root contains the same entity. Add Dock Back and idempotent
   native-close docking. Only after this works should the old direct
   `open_visualizer` path be removed.
5. **Persistence.** Save/restore Guise's encoded snapshot plus stable item
   descriptors, locations, and all native window bounds. Add corrupt-file,
   unknown-ID, duplicate-ID, and version-fallback tests.
6. **DAW workspace polish.** Make Material, Inspector, and later Mixer/History
   workspace items; add focus/split/zoom shortcuts, tab menus, empty/drop
   affordances, minimum useful pane sizing, and layout presets such as Edit,
   Analyze, Separate, and Loom.
7. **Titlebar consolidation.** Once pane behavior is stable, optionally render
   `PaneGroup` flush to the top with `.titlebar(leading, trailing)` and overlay
   transport/window controls in its reserved insets. Do not combine this with
   the first docking patch; the current transparent titlebar and 82 px traffic
   light inset already work.

## Verification

Pure tests should cover stable item allocation, default snapshot contents,
round-trip persistence, unknown/corrupt snapshot fallback, pinned-track close
policy, idempotent dock-back, and location transitions.

GPUI tests should assert that pane activation renders the registered entity,
tear-off removes it from the main group without dropping it, dock-back inserts
the same entity handle, closing a floating window docks exactly once, and
restored divider ratios/tab orders match the saved snapshot.

Manual checks are still necessary for native behavior:

1. Load audio, change zoom/follow and spectrum settings, edit Loom events.
2. Drag tabs among panes and onto all four split edges; resize dividers.
3. Float each visualizer, continue playback, then dock it back.
4. Confirm all state from step 1 survived and playhead updates remained shared.
5. Quit with two floating windows, relaunch, and verify window placement and
   dock topology.
6. Exercise space/arrow/lens shortcuts with focus in the main group and every
   floating window.
7. Repeatedly close/dock/float while HPSS or Loom work is in flight; stale
   generation results must remain rejected and no entity should be duplicated.

The compile spike validates the critical 1.5.3 signatures: item allocation,
weak render/title callbacks, event subscription, snapshot decode/restore,
deferred `open_window`, `on_window_should_close`, storing a typed window handle,
and adding the same item back to the focused main pane.
