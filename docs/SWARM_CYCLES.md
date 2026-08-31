# Swarm cycles: broad bricks, deliberate convergence

Status: active execution plan on 2026-08-31 against `main` at `50d61ce`.
This document refines [SWARM_PLAN.md](SWARM_PLAN.md) after a second full
architecture read of the live tree. `SWARM_PLAN.md` remains the detailed
workstream and acceptance-corpus index; this document owns the current cycle
shape and swarm mechanics.

## The two products remain equally strong

audec is an excellent electronic-music DAW and an unusually honest reverse
production system. Neither is a delivery vehicle for the other.

The production loop optimizes latency from intent to sound: import, select,
sample, arrange, sequence, automate, mix, save, reopen, and continue without
fear. Provenance is quiet texture until requested.

The interpretation loop makes a finished recording debuggable:

```text
source = sum(explanations) + residual
```

Evidence produces competing explanations; accepted explanations become
ordinary editable construction; construction is rendered through the same
engine as authored music; residual and excess make failure audible and
visible. Coverage measures reconstruction, never correctness.

The shared substrate is the product advantage: selections become samples,
rhythm evidence becomes patterns, measured motion becomes automation, edits
become experiments, and a project can eventually travel as a reading of a
recording.

## What changed after reading the implementations

The earlier campaign correctly identified the feature families but made their
lanes too vertically self-contained. Several apparently small workstreams are
actually shared substrates:

1. **Commands are a domain-wide foundation.** `command.rs` cannot simply fill
   its `todo!()`. Every domain needs a history-free apply/inverse seam, allocator
   state must persist, runtime PCM must participate in the publication plan,
   and editor-local histories must be migrated without nesting undo stacks.
2. **Persistent render publication precedes tiling.** `AudioHost` currently
   accepts completed PCM and the UI replaces the entire host after a bounce.
   The first instrument-quality win is one persistent host that publishes a
   coherent new whole-bounce cohort at a safe or loop boundary. Master tiles
   then optimize a behavior that is already correct.
3. **Sampling needs a constructive object.** A slice is not honestly modeled
   as an unrelated imported file. It is a ranged material reference with a
   source asset, exact frame interval, canonical PCM identity, sampler zone,
   pad/kit identity, and explicit route. Manual sampling, onset chopping,
   deprojection, and explain-as-expression should all compile to the same
   constructive edit plan.
4. **Pattern expressions need runtime semantics.** `cycle_index` affects
   alternation, `every`, `fast`, and `slow`, but a static `StepPattern` freezes
   them at one cycle. Symbolic bindings and stable lane identity must survive
   across real placement cycles. Origin metadata also needs actual bindings,
   derivation, seed/cycle policy, and a monotonic divergence state.
5. **Explanation needs durable inputs.** Loom, HPSS, reconstruction proposals,
   and model results are currently lens-local or transient. A content-addressed
   artifact catalog and an interpretation domain must exist before a compiled
   explanation or comparison can reliably outlive its lens.
6. **Aspect geometry needs one correction before implementation.** A single
   `time × bands × channels × objects` Cartesian product cannot represent
   arbitrary unions. Concrete aspects should normalize to a union of regions.
   Signal layer (`Source`, an explanation, or a comparison residual) is
   distinct from geometric selection.
7. **The current fingerprint classes have different jobs.** Portable/slop-
   tolerant `GoldenFingerprint`, non-cryptographic asset duplicate hints, and
   exact cryptographic render/artifact digests must not be interchanged.
8. **Project persistence is not complete yet.** The envelope names external
   payloads without storing them, AIR has no constructive codec, restored
   aggregate state has no public hydration path, allocator high-water marks
   are incomplete, and autosave is a snapshot rather than a replayable journal.
9. **Guise is not the problem.** Same-entity dock/float/dock-back mechanics
   already work. The missing layer is dynamic persisted view identity, targeted
   pane descriptors, shared services, and one project session above the views.
10. **A full Cargo workspace move is not yet mechanical.** Extract portable
    seams now—workspace model, audio core/host adapter, project format/I/O,
    worker protocol—and perform the package move after command and pattern
    contracts stabilize.

## Target ownership

`DawProject`, not the legacy `project::ProjectDocument`, remains the canonical
validated aggregate. A UI-independent controller owns history, journaling,
runtime media, render publication, and snapshots; GPUI wraps it as one entity
per open document.

```text
ApplicationController
└── ProjectWindow
    ├── Entity<ProjectSession>
    │   ├── ProjectController
    │   │   ├── canonical DawProject
    │   │   ├── aggregate command history + journal
    │   │   ├── runtime media / PCM materializer
    │   │   └── published immutable read snapshots
    │   ├── ProjectAudioController / RenderService
    │   ├── AnalysisArtifactCatalog
    │   ├── InterpretationStore
    │   ├── SelectionService + Aspect/link groups
    │   └── tasks, diagnostics, document lifecycle
    └── WorkspaceRoot
        ├── global transport / status / tempo chrome
        ├── dynamic PaneGroup
        └── WorkspaceItemRegistry
            └── views own target IDs, viewport, tools, and gesture previews
```

`ProjectSession` owns no view/window entities. Workspace panes may hold the
session entity. Views never own editable copies of project truth. One gesture
previews locally and commits one command batch.

The render side converges on:

```text
LiveProjectSnapshot + deterministic EngineRecipe
                         ↓
                    RenderPlan
                         ↓
                 RenderCoordinator
            ┌────────────┼────────────┐
       playback cohort  export   explanation/coverage
            ↓
   persistent renderer-backed AudioHost
```

The interpretation side converges on:

```text
Aspect + SignalLayer → AIR facts/query
          │
          └→ ExplanationDefinition + ArtifactRef
                       ↓ compile
                 source / construction / residual
                       ↓
              coverage + ComparisonObservation
                       ↓
              InterpretationStore → ReadingDocument
```

## Cycle anatomy

Every campaign cycle has four phases. This replaces the expectation that each
feature lane independently produces a polished mini-product.

### 1. Contract snapshot

The coordinator freezes only the narrow shared vocabulary required by the
cycle and copies current signatures into lane briefs. The contract may be a
new module or DTO designed explicitly for later convergence.

### 2. Brick wave

Agents own disjoint files or disjoint new modules and build substantial pieces
in parallel. A brick lane owes:

- coherent types/algorithms or a large adapter with an explicit boundary;
- an interface/deviation report for the convergence writer;
- focused tests where they accelerate the work;
- no unrelated edits, staging, stashing, or opportunistic refactors.

A brick lane does **not** owe a whole-tree build or full workflow proof. New
modules may remain deliberately unreferenced until convergence. Shared files
are assigned to one writer, but their dependent fallout may wait.

### 3. Convergence wave

One or more integration agents reconcile interface drift, add module roots,
perform shared-struct/codec fallout, connect controllers and panes, and restore
one compiling tree. Convergence owns whole-tree formatting/build/testing and
produces named-file commits.

### 4. Checker and musician wave

A separate checker reads the actual diff and test statements, then exercises
the cycle's musical and explanatory workflow on real material. It hunts for
mirrored local state, demo-backed success, inaudible edits, stale render
cohorts, weak provenance, fake identity, and UI actions that bypass commands.
Only this verdict promotes a cycle checkpoint to `main`.

## Six broad cycles

Cycles are dependency-shaped, not calendar-shaped. A cycle may repeat or split
when a foundational interface is still teaching us something.

### Cycle 1 — lay the rails and forge the bricks

This is the widest architecture wave. The existing application remains usable
while most work lands in new modules or history-free domain seams.

**Project/command front**

- history-free apply/inverse and allocator-state APIs in arrangement,
  sequencer, automation, mixer, assets, bindings, and AIR;
- revision-independent `CommandBatch` inside revision-checked
  `CommandEnvelope`, typed `IdClaim`, address-aware coalescing, and
  before/after-derived `ChangeSet`;
- `project_controller`, command journal/codec, and GPUI project-event shells;
- migration ownership per domain: `LegacyMirror` or `CommandOwned`.

**Document front**

- checkpoint/package product, raw unknown-section retention, complete AIR
  codec, allocator/revision preservation, aggregate hydration, and media
  resolver contracts;
- `DawProject::from_parts` / restored `LiveProject` seam;
- revision-guarded mark-saved semantics.

**GPUI/workspace front**

- `ProjectSession`, audio controller, selection service, view-link facets,
  project task/diagnostic types, action registry, and project chrome contracts;
- dynamic `WorkspaceItemDescriptor { id, kind, target, state, link_group }` and
  persisted monotonic view allocation;
- pure arrangement gesture kernel, waveform-proxy service, typed UI drag/drop
  intents, pattern lifecycle builders, and pad-view component.

**Render front**

- `RenderPlan`, plan identity, scope, render products, exact digests,
  determinism grade, status/health, and coherent publication cohorts;
- renderer-backed persistent host seam and signed timeline mapping;
- tile store/scheduler harness whose kernel may initially be a fake or whole
  bounce; no premature tail approximations.

**Language/interpretation front**

- corrected region-based Aspect + separate signal expression/layer;
- curve expressions, full AIR fact index/query, artifact identities,
  explanation/comparison definitions, pure coverage/excess math, and reading
  in-memory/codec foundations;
- exact render digest alongside `GoldenFingerprint`.

**Worker front**

- real JSONL DTO codec/session state, process supervisor, artifact store,
  typed mixed-result manifest, fake worker, model-claim bundle, static catalog,
  and Beat This/IDM adapter contracts without downloading weights yet.

**Seam extraction front**

- portable workspace model separated from GPUI host;
- audio core separated from Rodio host;
- project-format DTOs separated from filesystem I/O;
- a library root/layer façade may land, but no full repository file move yet.

Cycle convergence freezes the small vocabulary needed by later waves and
returns the whole tree to green. Visible product behavior need not yet change.

### Cycle 2 — authoritative document and docked project shell

- integrate domain command kernels into `ProjectController::{execute, undo,
  redo, commit_gesture}` and publish cached immutable snapshots/events;
- put `LiveProject` behind the controller and migrate compatible domains from
  reconcile ownership without allowing stale mirrors to overwrite commands;
- mount a new `ProjectWindow`/`WorkspaceRoot` beside the compatibility
  Workbench; dock existing arrangement, steps/piano, mixer, automation, asset
  browser, overview, and analysis panes through dynamic descriptors;
- add a stable action/focus routing surface and global project chrome;
- publish coherent completed whole-bounce revisions into one persistent
  `AudioHost`, preserving transport and switching complete loop cohorts at
  wrap boundaries;
- connect checkpoint/save/open/recovery services beneath GPUI, even if their
  first UI is minimal;
- land ranged sample material, kit/zone/route identities, pattern origin and
  real-cycle provider contracts, with complete codecs.

The convergence gate is one authoritative document, aggregate undo, a full
docked production shell, and uninterrupted whole-bounce publication.

### Cycle 3 — make the desk genuinely playable

- selection → sample or onset-chop → pads/kit → step pattern → canonical
  arrangement placement → audible project-derived sampler route;
- pad audition, targetable pattern panes, ordinary pattern lifecycle,
  drag asset→pad/track, notation entry, real cycle-index evaluation, and
  monotonic divergence after grid edits;
- pointer-based arrangement move/trim/slip/fade/loop, marquee, tools, track
  management, and source-aware waveform proxies;
- Save/Open/Save As/Recovery/Export GPUI actions, dirty state, missing-media
  diagnosis, and workspace restoration;
- persistent-host master tiles with coherent revision cohorts, starvation
  status, and whole-render oracle comparisons;
- executable fake-worker path and artifact publication.

Musician gate: starting from real audio, make and reshape a beat in under a
minute, loop while editing, save, reopen, and export the same frozen render.

### Cycle 4 — tactile motion and reverse/forward fusion

- real mixer meter snapshots, draggable/coalesced controls, routing/groups/
  returns, automation binding/creation, inline lanes, and audible parameter
  changes through the same render service;
- rhythm families/pattern hypotheses preview and apply through the constructive
  planner into ordinary samples, pads, patterns, routes, evidence, and inverse;
- pattern explanations as anonymous symbolic terms ranked by description
  length and fit, with exact-audio fallback remaining distinct;
- artifact catalog, persisted reconstruction-application records, DAW solo
  render selection, compiled explanations, aligned source/construction/
  residual products, and persistent comparisons;
- shared Aspect/link facets across production and analysis panes;
- Beat This worker integration as a competing claim beside native rhythm;
- perform the full Cargo workspace split here only if command/pattern/project
  format interfaces have stabilized; otherwise defer one cycle.

Gate: deproject a mixed beat, edit the resulting construction as music, hear
the changed residual immediately, and undo the entire promotion atomically.

### Cycle 5 — interpretation platform and shareable readings

- explained/excess energy tiles and coverage view, always paired with residual
  audition and separated visually from evidence/confidence;
- persistent comparison strip and evidence inspector everywhere an
  explanation appears;
- reading export/import/verification/diff/merge planning with strong source
  digests, reading-qualified typed identities, unknown-section retention, and
  merge-as-coexisting-hypotheses;
- AIR query UI and residual-guided queries;
- explain-as-expression for rhythm and modulation/curve evidence;
- headless command/query/render protocol and CLI;
- IDM mixed-result worker, then larger separation/pitch/sample/synth workers
  only after the supervisor and artifact semantics hold.

Gate: exchange a reading without source audio, verify it against matching
material, audition its failures, compare alternatives, and import it as one
undoable plan without renumbering foreign identity.

### Cycle 6 — convergence, delight, and retirement

- complete menus, command palette, contextual actions, shortcuts, keyboard
  editing, exact numeric entry, accessibility, focus/pointer-capture behavior,
  dynamic window/layout persistence, and calm activity/diagnostic surfaces;
- deepen clip/pattern/mixer/automation manipulation until ordinary production
  no longer feels like a prototype;
- optimize render cohorts, tile history/checkpointing, bus/stem products,
  streaming export, and large-project behavior without forking the engine;
- finish package boundaries and retire the fixed atlas, legacy persistence,
  legacy session arrangement, normal-path Workbench ownership, and deep-diff
  reconciliation only after their replacements are exercised;
- run the complete LMMS-parity musician corpus and reverse-production corpus
  on Silent Shout and synthetic adversarial fixtures.

This is not a cleanup-only tail. It is the cycle where audec earns trustworthy
muscle memory and the architectural scaffolding disappears from the user's
experience.

## Single-writer and convergence map

The central collision points remain single-writer during a brick wave:

| File | Convergence responsibility |
| --- | --- |
| `src/ui.rs` | compatibility façade and final root handoff |
| `src/main.rs` | module roots/bootstrap; coordinator only |
| `src/live_project.rs` | controller/render-facing ownership migration |
| `src/daw_project.rs` | aggregate domains, bindings, validation, restoration |
| `src/command.rs` | envelope vocabulary and aggregate dispatch |
| `src/project_codecs.rs` | shared-struct/persistence fallout |
| `src/workspace.rs` / `src/workspace_ui.rs` | descriptor model and Guise runtime mapping |
| `src/sequencer.rs` | pattern origin/cycle/allocator semantics |
| `src/daw_engine.rs` / `src/audio.rs` | render-plan and persistent-renderer seams |

High-value freely parallel new modules include:

```text
project_controller.rs       project_session.rs        project_selection.rs
project_audio_controller.rs view_links.rs             ui_actions.rs
workspace_items.rs          arrangement_interaction.rs waveform_proxy.rs
ui_drag.rs                  constructive.rs           sample_material.rs
sample_kit.rs               pattern_runtime.rs        pattern_explain.rs
project_format.rs           project_store.rs          media_resolver.rs
render_plan.rs              render_products.rs        render_service.rs
render_tiles.rs             air_facts.rs              artifact_catalog.rs
explanation.rs              interpretation.rs         comparison.rs
coverage.rs                 reading.rs                model_wire.rs
model_supervisor.rs         model_store.rs            model_claim.rs
```

The exact names may change during convergence. Their dependency direction may
not: GPUI and device adapters sit above project, render, interpretation, and
worker services; no lower layer reaches upward for a window or widget.

## Campaign refusal set

- No second playback/export engine.
- No mutable in-process general-purpose scripting environment.
- No view-owned clone becomes project truth.
- No fake instrument/source identity from a family name or model label.
- No coverage scalar is presented as correctness.
- No content cache relies on a tolerant fingerprint.
- No save/open path silently drops unknown data or allocator state.
- No stale or hybrid render cohort is presented as the requested revision.
- No full crate split is allowed to consume a cycle before the seams are real.
- No demand that every exploratory brick independently prove the assembled
  application; convergence and checker waves exist precisely for that work.
