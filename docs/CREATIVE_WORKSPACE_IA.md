# Creative workspace information architecture

Status: Cycle 10 implementation audit and convergence contract
Scope: the reachable GPUI application after Cycles 6–9
Companion product intent: `PRODUCT_INFORMATION_ARCHITECTURE.md`
Evidence baseline: the current source tree, plus a live Cycle 9 run on “Like a
Pen”

This document records what the application actually is, where its creative
objects and interaction state currently live, and which authorities must be
removed to make the desk coherent. It is deliberately stricter than a feature
inventory. A pane is not integrated merely because it receives a project
snapshot, and a result is not usable merely because a typed receipt exists in
the core.

## 1. Cold-eye verdict

The application has one increasingly sound command and audio core surrounded by
several overlapping product shells.

The good center is real:

- `ProjectSession` owns the installed `LiveProject`, command history, the
  published project snapshot, project selection, link routing, and published
  audio status.
- `ProjectAudioController` and one `AudioHost` own project rendering,
  transport, scoped timeline audition, and the preview bus.
- `ObjectRef`, reveal receipts, `ObjectNavigator`, and
  `receipt_navigation.rs` already provide most of the typed completion
  vocabulary.
- Dynamic pane descriptors carry stable view IDs and typed targets.

The visible desk does not yet consistently obey that center:

1. `ProjectSelection` cannot name several first-class `ObjectRef` variants,
   including Instrument, Pad, Finding, Explanation, Comparison, and Reading.
   The product therefore cannot express “the pad visibly selected in the
   Sampler is the selected creative object.”
2. `DawWorkspace` keeps `ExplorerSelection` and `InspectorReport` beside, not
   as projections of, session selection. Inspector truth follows an Explorer
   click, not the object visibly targeted by the active editor.
3. Most editors retain a cloned mutable-looking domain snapshot. Arrangement,
   Pattern, Mixer, Automation, and Sampler submit commands correctly, but not
   all local selections, targets, view states, or creation completions return
   to the session and workspace authorities.
4. Analysis panes are still independent calculators over Workbench analysis.
   Their results and edits are pane-local and vanish with the entity. The
   durable interpretation, artifact, promotion, and reading cores are not the
   live source of those panes.
5. `DynamicWorkspaceRoot` still performs the real pane/window mutations.
   `WorkspaceSessionLayout` is updated afterward from snapshots, so the
   declared authoritative layout is currently a mirror.
6. Playback is one transport in the successful paths, but audible affordances
   are inconsistent: Browser/Sampler and analysis previews work; aligned
   analysis/comparison audition works; Pattern cycle and piano audition are
   visibly offered but have no host adapter.

The corrective direction is consolidation and deletion. Do not add a
“creative context coordinator” beside these models. Replace the competing
selection, result, semantic-store, and workspace authorities with the existing
typed session/navigation boundaries.

## 2. Runtime topology that is actually reachable

The app launches a fixed product shell around a dynamic workspace:

```text
DawWorkspace
├── fixed Explorer rail
├── DynamicWorkspaceRoot
│   ├── Overview / source timeline (initial)
│   ├── Waterfall (initial)
│   ├── Rhythm (initial)
│   ├── Components (initial)
│   ├── Separation (initial)
│   ├── Loom (initial)
│   └── dynamically created Arrange, Browser, Pattern, Mixer,
│       Automation, analysis, Sampler, and reverse-object panes
└── fixed Inspector rail
```

`Open*` application actions create dynamic panes. The old
`Workbench::open_visualizer`, `open_arrangement_editor`,
`open_sequencer_editor`, `open_mixer`, `open_automation`, and `open_assets`
methods still create native windows, but their only ordinary launcher is the
Workbench material rail, which is hidden when hosted in `DawWorkspace`. They
are a dormant second navigation/window system, not a required compatibility
path in the running product.

Reverse panes are constructible by the factory for Finding, Explanation,
Comparison, and Reading targets. They are not currently discoverable from the
live Explorer: `refresh_product_shell` calls `ExplorerInput::project`, which
passes empty finding, explanation, comparison, and reading collections.
Moreover, live UI code clears `ReverseSurfaceStore` on project replacement but
does not hydrate it from an authoritative semantic store. A descriptor can
therefore open a typed but missing reverse surface.

Workspace kinds without a live factory mapping become `WorkspaceNotice`.
Notices are honest placeholders, not creative surfaces.

## 3. Pane-by-pane ownership and integration audit

“Local” below means presentation state that may legitimately belong to one
view. “Mirror” means project/session information copied into a pane and at risk
of becoming another authority.

| Reachable surface | Authoritative input | Local state | Project/session mirror | Selection and viewport path | Audible path | Creation/result path | Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **Overview / source timeline** | Workbench `Analysis`; `ProjectSession`; project renderer | timeline interaction, source viewport/follow, loop presentation, spectrogram tiles | Workbench also retains `ProjectState::Ready(Analysis)`, source-derived asset registry, playhead and selection fields | Publishes time/aspect/signal selection to `ProjectSession`; owns the only global zoom/loop shortcuts | global transport; no separate realtime graph | Make sample, Slice to kit, and Make beat use typed constructive receipts and automatic reveal | Keep as Signal/source surface. Move transport and completion status to global chrome; remove its hidden all-purpose side rails and redundant fields after adapters read the session directly. |
| **Arrange** | cloned `ArrangementEditor` from each project publication | clip/track selection, gesture preview, viewport/follow, ruler time selection, loop range | full arrangement snapshot in the editor | clip gesture selection is published to session; `ArrangementTimelineCallback` is never installed, so ruler time selection and loop are pane-local islands; descriptor view state is read only at creation | seek and playhead use global transport | edits call the revision-only arrangement executor; available receipt/reveal adapter is bypassed | Keep. Install timeline/session adapter, use `execute_arrangement_event_revealed`, and publish descriptor state. Remove the direct revision-only UI path. |
| **Browser / Media pool** | cloned `AssetRegistry` from project publication | filter/search/tag filter, selected asset, source range, chop preview | entire asset registry | asset selection stays inside Browser; activation merely opens Arrange and does not select/place/reveal that asset | exact range preview bus works | Make beat and sample actions publish and reveal; favorite mutation directly edits the cloned registry and is lost on replacement/save | Fold this detailed material editor into **Library** behavior or retain it as a targeted Material surface. Delete direct registry mutation and the meaningless “open asset = open Arrange” behavior. Do not keep both a global Library and an unrelated Media Pool selection. |
| **Pattern** | cloned `Sequencer` plus workflow hydration from project snapshot | note/step selection, mode, target, expression draft, preview cycle/seed, occurrence choice, gesture state | entire sequencer snapshot | does not consume or publish global creative-object selection; target changes stay in editor/descriptor bridge | cycle plans are produced but never consumed; piano audition callback is never installed | create/duplicate/delete/edit are authoritative workflows, but outcomes update only the editor; no global selection, Inspector update, or reveal completion | Keep. Make the targeted Pattern the global selected object, route creation through the existing revealed completion adapter, publish descriptor state, and connect one shared renderer audition adapter. Remove visible play controls until that adapter is installed. |
| **Mixer** | cloned `MixerGraph` from project publication | selected bus, gesture preview, meter cache | full mixer graph; no live meter publication is supplied by the UI host | selected bus is pane-local even though `ProjectSelection` can name a bus; target descriptor is only applied on construction | project playback is global; meter UI has no current post-DSP feed | create return/group/send and edits submit commands; new bus is not selected/revealed globally | Keep. Publish bus attention, install real meter products, and return typed mutation/reveal outcomes for created signal-flow objects. Delete legacy compatibility backends from live construction. |
| **Automation** | cloned `AutomationGraph` from project publication | lane/point selection, cursor, viewport, snap, write mode/series, gesture | full automation graph | selected lane/point and viewport are local; descriptor is only initial input | no independent playback; write/render status has no complete live host path | create lane submits a command, then snapshot reconciliation retains the old valid lane, so the new lane is not selected or revealed | Keep. Make lane creation land on the new lane, publish global container selection, and persist local viewport/write state. Use typed creation receipts. |
| **Sampler / Instrument** | cloned sample-kit library, assets, and mixer buses from project publication | target, selected pad/zone/bank, held gates, result selection | three domain snapshots | visible pad/zone target does not update global creative selection or Inspector; retarget may update workspace target but not the fixed shell | pad gates use the shared preview bus with generation-safe releases | map/edit/new-kit/new-pad actions are authoritative and sample receipts can reveal; there is no “create beat from selected pads” forward action in the Sampler | Keep as Instrument. Publish Instrument/Pad/Zone attention, show provenance in the global Inspector, and put the next musical action in this surface. Remove dependence on the Explorer footer for forward progress. |
| **Waterfall / Signal lens** | Workbench `Analysis` | time/frequency viewport, follow, FFT/window/db settings, locally recomputed spectral pixels | analysis source/path and computed spectral arrays | receives semantic selection but stores it without applying or visibly presenting it; has an independent viewport by design | seek uses global transport | no durable finding or artifact publication | Keep as a Signal lens. Bind selected aspect visibly; publish retained analyses as artifacts/findings or label them temporary. Persist recipe/view state. |
| **Rhythm** | Workbench analysis; pane-local `deproject_rhythm` task | analysis generation, family focus, viewport | full `RhythmDeprojection` result | family selection is pane-local; session selection is stored but unused | medoid excerpt uses preview bus; seek uses global transport | no retain, apply, or promote action despite a durable rhythm promotion core | Migrate to Findings backed by authoritative artifacts/interpretations. Add explicit Apply/Keep/Dismiss; remove this pane-local result as the final truth. |
| **Components** | precomputed Workbench NMF analysis | viewport | component decomposition | no component selection enters session | seek only; no component audition | no retain/promotion/result object | Treat as a temporary diagnostic lens or publish Findings. Do not present components as durable objects until they have scoped identities and receipts. |
| **Separation / HPSS** | Workbench PCM; pane-local transform task | selected span viewport, result generation | original/harmonic/percussive/residual PCM result | ignores delivered semantic selection; its analysis span comes from local viewport | aligned Source/Harmonic/Transient/Residual audition uses global transport and scoped audition correctly | no artifact, finding, sample, comparison, or promotion result | Keep transform machinery, migrate result to an explicit temporary comparison or persisted Finding. Do not let an auditionable result disappear without an Apply/Keep choice. |
| **Loom** | Workbench PCM/onsets; pane-local inference | selected cluster, event enable/timing/gain edits, span, result generation | full sequence sketch and rendered PCM | editing target is nearest event to global playhead, but the cluster/event is not a session selection | aligned mix/reconstruction/residual audition is correct; template uses preview bus | extensive edits remain pane-local; deprojection promotion exists in core but has no UI entry | Highest-priority orphan. Replace pane-local “editable” final state with a revision-pinned proposal, then Apply through `deprojection_promotion`; reveal the created Pattern/Instrument/Arrange object. |
| **Reverse Finding / Explanation / Compare / Reading** | `ReverseSurfaceStore` documents and pane-local comparison controller | chosen comparison/channel and feedback | semantic documents are held in a separate Workbench store; comparison semantics are rebuilt into a temporary `InterpretationStore`; artifact catalog starts empty | evidence reveal uses typed navigator; reverse objects cannot be represented by global `ProjectSelection` | exact comparison executor and scoped audition are correctly shared | explicit edit consequences state that lowering is not connected; comparison products are controller-local; reading import has no live route | Keep the surface, delete the separate semantic truth. Hydrate on demand from one session-owned interpretation/artifact/reading store; apply consequences via commands/promotions and revealed receipts. |
| **Fixed Explorer** | `ExplorerModel` rebuilt from `DawProject` only | mode, filter, selected node, breadcrumb, scroll | a second selection/revision record beside `ProjectSession` | Explorer clicks push some representable objects into session, but session/editor changes do not drive Explorer selection | none | footer duplicates source Make actions; reveal uses typed navigator | Keep the role, replace its selection authority. Feed all four modes from authoritative stores and make mode/filter/expansion the only Explorer-owned state. |
| **Fixed Inspector** | `InspectorReport` derived only when Explorer selects an object | scroll | cached report and breadcrumb | does not observe active editor target or session selection | none | reveals related fields but has no editing/action command adapter | Keep the role, drive it from global creative selection, and make unsupported object kinds explicit. It may be dockable, but there must be exactly one Inspector service. |

## 4. Concrete live contradiction

The Cycle 9 run on “Like a Pen” is the most useful acceptance example because
all displayed facts are individually true while the desk-level story is not:

- Sampler opened on Kit 1, Pad 1, with one exact 63,422-frame zone.
- Explorer correctly listed Instrument → Sample Kit → Pad 1.
- Patterns correctly said “No patterns yet.”
- Sampler visibly selected Pad 1, while Inspector still said “No object
  selected.”
- A persistent top completion strip still said “Instrument created” and
  offered Reveal even though the destination surface was already visible.
- The only apparent forward action was the unrelated Explorer footer’s Make
  beat command. Sampler itself did not offer “create a beat from these pads.”
- A coordinate click intended for completion Reveal hit the pad and produced
  “Released pad 1.” Native accessibility exposed only the window/menu, not the
  GPUI controls.

This is not a styling problem. It proves five model failures:

1. editor target and global object selection are different authorities;
2. destination activation is not treated as completion;
3. completion has no lifecycle tied to the active destination;
4. the creative next step is attached to a source-range footer rather than the
   created Instrument;
5. semantic input/focus is not represented in the native accessibility tree,
   making both keyboard and automation geometry-dependent.

The acceptance form of this journey is:

```text
create/slice → Instrument opens → new pad is global primary object
             → Inspector shows exact source frames and route
             → completion becomes destination-local status and expires
             → “Create pattern from pads…” is available in Instrument
             → every named control is keyboard and AX reachable
```

## 5. Creation, extraction, and promotion ledger

Every durable creation must end in a selected, inspectable `ObjectRef`. The
following ledger distinguishes core capability from live completion.

| User-visible or core flow | Durable result | Current landing | Orphan risk | Required migration |
| --- | --- | --- | --- | --- |
| Open audio / new source project | Material, source track/clip, AIR source binding | Overview; Explorer rebuilds | initial object selection remains empty | select the Material or source clip and show it in Inspector; keep Overview as destination, not completion authority |
| Overview Make sample | Instrument/Pad/Zone | automatic Sampler reveal plus persistent top strip | Inspector cannot name pad; banner outlives landing | extend global creative selection; acknowledge completion when destination applies target |
| Overview Slice to kit | Instrument with Pads/Zones | automatic Sampler reveal | same split selection; no next-step action in Instrument | same, plus Instrument-local Create Pattern action |
| Overview Make beat | Instrument, Pattern, occurrence, route | Pattern/Arrange according to receipt | strongest current path, but Inspector/Explorer attention can remain stale | make reveal set global primary and related objects; use one transient completion owned by destination |
| Browser Make beat | Instrument, Pattern, occurrence | request currently prefers a new Sampler | created Pattern is related but not the working target | ask/show After destination and honor it; default to Pattern when command says make beat |
| Browser favorite | intended Material metadata edit | cloned Browser registry only | **false success; not durable and lost on publication** | delete local mutation; add an asset command or remove the control |
| Browser Activate | no mutation | opens Arrange regardless selected material | selected asset becomes unusable context | replace with Reveal Material or explicit Place in Arrange; never open an unrelated surface silently |
| Sampler new kit/new pad/map/edit | Instrument/Pad/Zone and metadata | Sampler receipt and possible reveal | visible target not global selection; generic banner duplication | land on exact target, update Inspector, and consume completion in place |
| Pattern create/duplicate | Pattern | same editor changes target | usable locally but absent from global selection/completion | use `execute_pattern_action_revealed` or equivalent workflow receipt integration; select/reveal exact Pattern |
| Arrangement add track/drop/duplicate/split | Track or typed clip occurrence | refreshed Arrange snapshot | revision-only executor discards available reveal recommendation; new target may not be selected | adopt `execute_arrangement_event_revealed`; apply its selection and reveal in the same view |
| Mixer add return/group/send | Bus/routing edge | refreshed Mixer | no typed product completion or global selection | return/select `ObjectRef::Bus`; expose routing relation in Inspector |
| Automation create lane | Automation lane | refreshed Automation, usually with previous lane retained | new lane exists but is not the editing target | select/reveal new lane from exact command result |
| Rhythm analysis | scoped hypothesis/finding candidate | pane-local result | closes without a durable identity | Keep creates Finding/artifact; Apply uses rhythm promotion; Dismiss is explicit |
| HPSS separation | artifact/finding/comparison candidate | pane-local PCM | auditionable result disappears | label temporary and offer Keep/Make sample/Compare; persisted choice gets typed identity |
| Loom edit | source program/proposal | pane-local sketch | edits look authored but are not in project history or persistence | compile and Apply through revision-pinned deprojection promotion; no editable-looking final state outside history |
| Deprojection promotion | Pattern, Instrument, clips, automation, route, provenance | core only | fully capable but unreachable | add one proposal Apply sheet and reveal `PromotionResult.created`; do not recreate lowering in UI |
| Reverse edit consequence | typed consequence | explicit “not connected” message | no mutation; honest but dead-end | map consequence to command/promotion or omit the action |
| Comparison execution | comparison observation, coverage, render products | pane controller and temporary semantic store | not in Explorer/Inspector and not durable across project replacement | publish through authoritative InterpretationStore/ArtifactCatalog, then use existing comparison reveal recommendation |
| Reading import/query | Reading and qualified entities | core only | Explorer Readings is permanently empty in live app | session-owned reading index, import workflow, Reading reveal; never merge hypotheses implicitly |

`receipt_navigation.rs` already inventories 24 durable flows and supplies
adapters for many UI call sites that still use revision-only or detached
terminals. Adoption is preferable to another outcome abstraction.

## 6. Playback and audition topology

There is one valid project transport. Preserve it.

| Path | Transport relationship | Status | Decision |
| --- | --- | --- | --- |
| Overview play/pause/seek/loop | owns intents for shared `AudioHost` transport | authoritative | move controls/status into always-visible global chrome; Overview may still manipulate them |
| Arrange seek/playhead | shared transport | connected | keep; connect its loop/time-selection callbacks to session controllers |
| Analysis pointer seek | shared transport | connected | keep |
| HPSS/Loom aligned signals | scoped audition replaces the exact project span and seeks shared transport | connected and semantically named | keep |
| Compare Source/Construction/Residual | executor publishes exact products into scoped shared audition | connected when semantics/render plan exist | keep; persist semantic products and show global audition subject |
| Browser material one-shot | preview bus owned by same `AudioHost` | connected | keep as non-locating preview |
| Sampler pad gate | preview bus with owner/generation and exact release ticket | connected | keep |
| Rhythm family medoid / Loom template | preview bus | connected | keep, but label as excerpt/template rather than project playback |
| Pattern cycle | editor produces `PatternLoopAuditionPlan` only | disconnected visible play affordance | connect through shared renderer with exact occurrence/cycle, or hide the affordance |
| Piano key/selected notes | `PianoAuditionCallback` exists but UI host never installs it | disconnected visible play affordance | connect to routed instrument preview, or hide the affordance |
| `audition_audio` Workbench field | offline pinned export render cache, despite its name | not a second realtime transport | rename to `audible_export_audio` and retain only as an export pin/cache |

Preview must not silently locate. Scoped timeline audition may locate only when
the control says so. All audible states must continue to publish one global
subject/status through `ProjectAudioStatus`; pane-local “Auditioning…” text is
secondary feedback.

## 7. Selection, focus, viewport, and accessibility

### 7.1 Replace the selection split

The current authorities are:

```text
ProjectSession.ProjectSelection
    primary: SelectableId (cannot name several creative objects)

DawWorkspace.ExplorerSelection
    selected ExplorerNodeId + selected project revision

DawWorkspace.InspectorReport
    refreshed from Explorer selection only

each editor
    local target/selection, sometimes published, often not
```

The target is:

```text
ProjectSession creative attention
    primary creative ObjectRef
    related creative ObjectRefs
    exact time/aspect/signal geometry
    optional editor-local detail owned by the active target pane

Explorer
    mode + filter + expansion + scroll only
    selected row is a projection of session primary ObjectRef

Inspector
    pure projection of session primary/related ObjectRefs and project stores

editor
    target must publish primary ObjectRef on user selection/activation
    note/step/point/zone detail may remain local but cannot contradict primary
```

This should replace, not wrap, the object portion of `ProjectSelection`.
Fine-grained `SelectableId` values for note, step, and automation point may
remain a subordinate editing selection, but they cannot be the only global
identity when the product object is a Pattern, Automation lane, Instrument, or
Pad. `ExplorerSelection.selected` should be removed after mode/filter/expansion
state is separated.

Reveal is the canonical state transition: validate receipt, resolve the
current object/predecessor, mutate authoritative workspace layout, activate
the destination entity, apply its exact target/subselection/scroll, publish
global creative attention, and let Inspector derive. A completion strip may
report diagnostics, but it must not remain the user's only way to finish the
transition.

### 7.2 Keep viewports local, but persist them

Independent viewports are useful and should not be globally synchronized by
default:

- Overview source timeline has a local frame viewport and follow flag.
- Each Arrange pane has its own frame viewport/follow/snap.
- Each analysis pane has independent time/frequency/recipe/follow state.
- Automation has its own coordinate window.
- Pattern has its own note/step viewport and preview cycle.

The defect is not independence; it is that editor changes are not written back
to `WorkspaceViewDescriptor.state` or `PanePresentationMemory`.
`WorkspaceSessionLayout::update_view_state` and
`update_presentation_memory` are never called by live UI code. Saved workspace
state therefore mostly reflects creation defaults/imported values, not the
desk the user arranged. Add per-pane state publication at gesture boundaries
or with debounce, then delete ad hoc persistence mirrors.

Linked-view facets may intentionally share geometry or signal, but receiving a
linked selection must not mutate viewport unless the descriptor opts into that
facet. The current Visualizer behavior of preserving its viewport is correct;
its failure is that the delivered selection is neither displayed nor applied
as semantic attention.

### 7.3 Focus and accessibility are product state

`ProductInputController` currently models a small logical snapshot of visible
workspace panes and close-prompt buttons. It does not expose the Explorer
tree, Inspector fields/actions, transport, completion actions, Sampler pads,
or editor controls to native accessibility. Runtime evidence shows only the
window and menu in the native AX tree.

Required convergence:

1. Make every interactive GPUI control a semantic node with stable identity,
   role, label, state, and default action.
2. Represent Explorer and Inspector, not only workspace regions.
3. Route keyboard focus and native AX focus through the same
   `ProductInputController` target; do not maintain a geometry-only shadow.
4. On pane activation, focus the meaningful selected object/control inside the
   existing entity, not merely the host region.
5. A modal close prompt traps focus; leaving restores the exact prior semantic
   target.
6. Acceptance tests must locate and invoke controls semantically. A coordinate
   click that can turn Reveal into “release pad” is a failed interaction
   contract even if pointer hit-testing is internally correct.

## 8. Explorer, Inspector, and workspace authority

### 8.1 One Explorer, not Explorer plus Media Pool

The fixed Explorer already presents the intended Project, Library,
Investigate, and Readings modes, but only Project/Library have data because its
input is `DawProject` alone. The dynamic Browser contains the detailed material
workflow but owns another asset selection and filter.

Converge them as follows:

- Explorer remains the singleton navigation/search service.
- Library rows use the authoritative asset registry and publish Material
  selection.
- Exact range/chop editing becomes either an expandable Library detail or a
  targeted Material editor pane. It must receive the same `ObjectRef::Material`
  target and selection.
- Remove `WorkspaceKind::Browser` as a generic duplicate once the Material
  surface exists, preserving persisted descriptors through one migration.
- Project, Investigate, and Readings are populated from the same session-owned
  object graph, never by scraping visible panes.

### 8.2 One Inspector service

The fixed Inspector is read-only and Explorer-driven. `WorkspaceKind::Inspector`
is otherwise a notice. Choose one lifecycle and implement it fully: a singleton
dockable Inspector descriptor/entity whose job remains stable whether docked,
floated, collapsed, or reopened. The shell may reserve its normal location,
but it must be represented in `WorkspaceSessionLayout`, not rendered as an
untracked fixed rail while an Inspector workspace kind also exists.

Inspector reads global creative attention and authoritative stores. It does
not retain an `InspectorReport` as independent state across publications.
Editing fields submit commands. Related-object Reveal uses `ObjectNavigator`.

### 8.3 `WorkspaceSessionLayout` must act first

Current live order is approximately:

```text
DynamicWorkspaceRoot mutates DynamicWorkspaceModel / opens native window
→ on_snapshot exports WorkspaceDocument
→ DawWorkspace replaces WorkspaceSessionLayout.document
```

This is mirroring, not authority. The target order is:

```text
intent (create/move/tear off/dock/hide/reopen/focus/state)
→ WorkspaceSessionLayout transition
→ apply binding + native-window effects
→ translate affected layout to Guise
→ retain same WorkspaceViewId and editor Entity
→ persist exported authoritative document
```

Delete direct layout mutations from `DynamicWorkspaceRoot` after each operation
has an authoritative transition. Moving or floating must not recreate an
editor, restart analysis, detach session binding, stop audition, or change
selection. Hidden/removed lifecycle remains explicit. Explorer and Inspector
must use the same transition path.

## 9. Authoritative creative object graph

The constructive half already lives in `DawProject`: arrangement, sequencer,
automation, assets, mixer, sample kits, AIR, and typed bindings. The
interpretive half is fragmented among Workbench `ReverseSurfaceStore`, a
temporary `InterpretationStore`, an empty `ArtifactCatalog`, pane-local
analysis results, and standalone Reading files/plans.

ProjectSession should own or install one coherent semantic publication beside
the constructive snapshot:

```text
ProjectSession
├── LiveProject / DawProject publication
├── InterpretationStore publication
├── ArtifactCatalog publication
├── Reading index and verification publications
├── creative attention (ObjectRef + geometry/signal)
├── history / diagnostics
└── audio status
```

Reverse surface documents then become projections assembled on demand, not an
authoritative store. Explorer Investigate/Readings, Inspector, Compare, query,
promotion, save, and reverse panes read the same publication. Applying an
interpretation command or publishing a comparison/coverage/reading result
emits a semantic publication and a reveal recommendation. Project replacement
cancels jobs by owner/generation and replaces the whole semantic publication.

AIR hypotheses remain epistemically distinct from authored objects.
`ObjectRef::Finding` can point to scoped evidence/proposals; Apply creates
ordinary DAW objects through a command receipt and retains provenance links.
Selection or naming never performs promotion.

## 10. Deletions and migrations, in order

These are replacement steps, not invitations to preserve both paths.

### P0 — make the current desk truthful

1. Replace global object attention so it can name every live `ObjectRef`, then
   make Sampler, Pattern, Browser, Mixer, Automation, Arrange, and reverse panes
   publish their targets. Derive Explorer highlight and Inspector from it.
2. Connect or hide Pattern piano/cycle audition controls. No visible play
   affordance may terminate at a stored plan/status string.
3. Delete Browser's direct favorite mutation or route it through a durable
   asset command.
4. Delete Browser Activate's unconditional “open Arrange” behavior; replace
   with typed Reveal/Place.
5. Consume completion when target activation and exact selection succeed.
   Keep only revision/staleness diagnostics in a transient global strip.
6. Add semantic/AX nodes for the entire reachable shell and editors.

### P1 — adopt existing receipt and layout authorities

1. Replace UI calls to revision-only arrangement execution with
   `execute_arrangement_event_revealed`.
2. Route Pattern creation/duplication outcomes through the existing revealed
   receipt adapter while preserving workflow hydration and exact cycles.
3. Return/select typed objects for control-surface creations.
4. Route create/move/float/dock/hide/reopen/focus through
   `WorkspaceSessionLayout` first; make DynamicWorkspace a renderer/adapter.
5. Publish descriptor state and pane memory from every live editor.

### P2 — remove duplicate shells and stores

1. Delete dormant Workbench native `open_*` paths and cached legacy editor
   entities after dynamic parity; do not convert them into navigator wrappers.
2. Migrate Browser descriptors to targeted Material/Library behavior and
   remove the duplicate generic Media Pool surface.
3. Implement one dockable Inspector service and remove the fixed/untracked
   versus notice duality.
4. Replace `ReverseSurfaceStore` as truth with projections over the one
   interpretation/artifact/reading publication.
5. Remove Workbench mirrors (`asset_registry`, legacy selection/playhead
   copies, and misnamed export cache) as their consumers move to session
   adapters. Retain caches only where they are immutable render products.

### P3 — finish the reverse-to-production loop

1. Turn Rhythm, HPSS, Components, and Loom results into explicitly temporary
   proposals or persisted Findings.
2. Wire Keep/Compare/Apply/Dismiss. Apply uses rhythm/deprojection promotion
   cores and exact reveal receipts.
3. Publish comparison observations/coverage and Reading imports into the
   authoritative semantic publication.
4. Put “Create pattern from selected pads…” in Instrument and remove the
   Explorer source footer as the only forward-production bridge.

## 11. Acceptance gates

The workspace is coherent only when all of these hold in the live app:

- Selecting a pad in Sampler highlights the same object in Explorer and shows
  its zone/source/route in Inspector without an Explorer click.
- Creating any object in `durable_reveal_rules()` either lands on that exact
  current object or reports a typed refusal/predecessor. No result ends at a
  revision string.
- Arrange add/drop/duplicate/split and Pattern create/duplicate select their
  exact results.
- Automation lane and Mixer bus creation leave the new object active and
  inspectable.
- Every audible button either produces a named preview/scoped audition through
  the shared host or is disabled with a visible typed reason.
- A saved/reopened workspace restores targets, viewports, follow state, mode,
  focus memory, and native window placement from `WorkspaceSessionLayout`.
- Moving a playing pane between main and floating windows preserves its entity,
  target, selection, analysis task, and transport.
- Investigate and Readings modes show authoritative semantic objects even when
  no corresponding pane is open.
- Loom Apply creates ordinary project objects in one undo entry and reveals
  them with provenance back to the proposal.
- The native AX tree exposes Explorer nodes, Inspector fields/actions,
  transport, completion action, pane tabs, Sampler pads, and editor commands;
  all are invokable without coordinates.
- The “Like a Pen” sample journey reaches Pattern creation from the selected
  Instrument without returning to an unrelated source-range footer.

## 12. Architecture-module decision for this wave

No new Rust architecture module is added in this architecture-only wave.
Adding `CreativeContext`, `CreativeOutcome`, or another selection coordinator
beside the current types would formalize the duplication this audit requires us
to delete. The correct reusable pieces already exist:

- `ObjectRef`, `RevealRequest`, `RevealRecommendation`, and `ObjectNavigator`;
- `ProjectSession` and `ProjectSelection`;
- `receipt_navigation` completion adapters;
- `WorkspaceSessionLayout` transitions;
- `ProjectAudioController`, preview owners, and scoped audition.

The adoption wave should change ownership and call sites, and extend/replace
the object portion of `ProjectSelection`, before extracting any new module. A
new module is justified only if it becomes the sole home of that replacement
and deletes the old authorities in the same change.
