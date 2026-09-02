# Authoritative workspace/session layout integration

`workspace_session_layout.rs` is the GPUI-neutral state machine for a workspace
attached to one `ProjectSession`. It builds on `WorkspaceDocument`; it does not
replace that portable document or introduce another pane-ID allocator.

## Ownership

The application root owns exactly one of each:

```text
ProjectSession entity                  project, selection, link groups, audio status
PaneSessionBinding                     addressed project/selection/audio fanout
WorkspaceSessionLayout                 durable pane/window/focus/presentation state
WorkspaceViewId -> editor entity       runtime entity map
WorkspaceWindowId -> native handle     runtime window map
```

`PaneInstanceId` is a semantic wrapper around `WorkspaceViewId`. Moving a pane
between dock trees or native windows must move the existing entity handle. It
must not allocate another view ID, recreate the editor, or restart its analysis
task.

`WorkspaceSessionLayout::session_id()` never changes. A second project gets a
second layout/controller; floating windows belonging to one layout cannot be
adopted by another session.

## Bootstrap

1. Load the persisted `WorkspaceDocument`.
2. Construct `WorkspaceSessionLayout::from_document(session_id, document)`.
3. Materialize one editor entity for every descriptor, including hidden ones
   when cheap; otherwise retain a factory keyed by the descriptor.
4. Apply every `initial_binding_effects()` entry to the shared
   `PaneSessionBinding` and `ProjectSession`. Each attach returns a full current
   project/selection/audio delivery, so a late-created pane never waits for a
   future event.
5. Translate the layout's `DockLayout` trees into Guise snapshots. Guise IDs
   remain process-local translations of stable view IDs.

The existing `DynamicWorkspaceModel` should become a Guise translation/cache
over `WorkspaceSessionLayout::export_document()`. It must not independently
decide pane location, focus, semantic links, or close policy.

## Applying transitions

Every model mutation returns `WorkspaceLayoutTransition`:

- apply `PaneBindingEffect::Attach`/`Detach` to the one pane-session binding;
- open, close, or focus native windows from `NativeWindowEffect`;
- rebuild only the affected Guise tree from the now-authoritative document;
- retain every `WorkspaceViewId -> Entity` entry across moves;
- persist `export_document()` after debouncing divider, window-bound, focus,
  view-state, or scroll-state changes.

Translate Guise split/divider snapshots with `replace_window_layout`; translate
native move/resize/mode notifications with `set_window_placement`. Both are
durable presentation updates and deliberately emit no binding or audio effect.
Cross-window tab movement goes through `move_pane` (or `tear_off_pane`) as one
atomic model operation instead of independently replacing two dock trees.

Moving, tearing off, docking, and native-window close produce no binding churn.
They are presentation changes over one playing session. Ordinary tab close
retains the descriptor and emits a detach; reopen emits attach and therefore a
fresh full-state delivery. Permanent descriptor deletion is a separate action.

Do not invoke pane selection callbacks while applying an addressed linked
selection. Prefer a non-publishing entity setter; otherwise consult
`PaneSessionBinding::is_selection_delivery_echo` before publishing.

## Focus and state

GPUI `FocusHandle`s remain window-local. When a `Focus` effect arrives, focus
the already-existing editor entity in the named window. Semantic selection is
not changed by focus.

Editors publish their typed `EditorViewState` and `PanePresentationMemory`
back to the model. Viewport/follow/recipe state remains in the descriptor;
generic scroll offsets, focus region, and reopen anchor live in the typed
workspace-session metadata stored under the document extension key. Both
survive JSON round trips and pane moves.

## macOS titlebar safety

For every main or floating window, measure the native traffic-light rectangle
when the platform exposes it and call `resolve_titlebar_layout`.

For transparent overlay titlebars:

- pass `guise_titlebar_insets()` to `PaneGroup::titlebar`;
- keep content top inset at zero;
- use `draggable_height` for the native drag strip;
- never also apply the leading inset to every split pane—Guise reserves it on
  the top-left titlebar only.

If measurement is unavailable, the contract uses a 70 logical-pixel macOS
traffic-light trailing edge plus the requested clearance, matching the current
82 px safe area without making that magic number part of the renderer.

For content-below-titlebar composition, apply `content.top` and do not reserve
traffic-light space inside the pane tab strip.

## Shutdown

Serialize the authoritative document before setting the shutdown guard. Native
window close callbacks during shutdown must not call `dock_window_on_close`, or
the saved multiwindow layout will collapse into the main window. During normal
operation, native close calls `dock_window_on_close`; it preserves pane entities,
bindings, transport, and editor state and emits one idempotent native close.
