# FOR GROK: how to understand, change, and verify audec

This file is an operational and conceptual briefing for an implementation
agent working on audec. Read it completely before making a change. Do not skim
only the task checklist. The concepts in the first half explain why the rules
in the second half exist.

The instructions are intentionally explicit. If a requested change appears to
conflict with this file, do not guess. Inspect the current tree, identify the
conflict precisely, and report it before weakening an invariant.

## 0. Start here every time

The canonical repository is:

```text
/Users/ember/dev/audec
```

The integration branch is `main`. The primary remote is `origin`, currently:

```text
git@github.com:emberian/audec.git
```

At the time this file was written, the clean pushed checkpoint was:

```text
e1e6bdf Prepare analysis panes off the UI thread
```

That hash is historical context, not permission to reset the repository. The
tree may have advanced. Always begin by running:

```sh
cd /Users/ember/dev/audec
git status --short
git branch --show-current
git log -5 --oneline --decorate
git remote -v
```

Interpret the result literally:

- If the working tree is clean, continue.
- If files are modified or untracked, assume they belong to the user or another
  active agent. Read the diffs before touching overlapping files.
- Never discard, reset, overwrite, or stash work merely to obtain a clean tree.
- Never use `git reset --hard`, `git checkout -- <file>`, `git clean -fd`, or a
  recursive deletion to solve an integration problem.
- Do not create a new worktree or clone in `/private/tmp` for an ordinary task.
  The project deliberately converged back to one canonical checkout because
  worktree and target-directory sprawl consumed a great deal of disk space.
- Do not assume the checkpoint above is still `HEAD`; trust the commands you
  just ran.

Before editing, state one end-to-end acceptance sentence. Example:

> Selecting a source range, choosing Beat, editing the resulting pattern, and
> pressing Play must reveal and audibly render the created objects through the
> same project revision and transport.

If the sentence ends at “a struct exists,” “a button appears,” “a revision
number changes,” or “a status message is shown,” the task is not yet framed as
a product outcome.

## 1. What audec is

audec is two full-strength products sharing one substrate:

1. It is an electronic-music production environment. It must become excellent
   at the ordinary musician loop: import, select, sample, play, sequence,
   arrange, automate, route, mix, save, reopen, and export. Low latency of
   intent-to-sound and trustworthy editing matter as much as novel analysis.
2. It is a reverse-production and music-decompilation instrument. It turns a
   finished recording into evidence-linked, editable explanations which can
   be rendered and compared with the exact source.

Neither product is subordinate to the other. The DAW is not merely a demo
surface for analysis. The reverse system is not merely a collection of
visualizers attached to a conventional DAW. The same material, project
objects, renderer, transport, commands, identities, and provenance connect the
two directions.

The compact model is:

```text
source = sum(explanations) + residual
```

An explanation may be a pattern, sample program, note sequence, curve,
reconstruction, mask, routed construction, or another renderable claim. The
residual is the audible error left after subtracting the construction from the
source. An additional **excess** channel represents energy introduced by an
over-explanation. Residual and excess are not generic confidence scores; they
are evidence that the construction fails in different ways.

This equation is audec's distinctive debugging loop:

```text
listen
  -> isolate an aspect
  -> inspect measurements and alternatives
  -> form or select a claim
  -> deproject it into editable musical structure
  -> render it with the ordinary production engine
  -> hear source, construction, residual, and excess
  -> edit and repeat
```

The reverse direction is credible only if the forward renderer is real. The
forward direction is more useful because every derived object can retain an
honest answer to “where did this come from?”

### 1.1 The three promises

Every substantial campaign should advance these three promises:

- **Instrument:** the shortest reliable path from musical intent to sound.
- **Decompiler:** competing, evidence-linked constructions whose audible error
  remains inspectable.
- **Medium:** portable readings, commands, queries, and generator terms that
  let people exchange interpretations without flattening provenance.

“Debugger for finished music” is a useful description of the second promise,
but it must never narrow the whole product. A cycle which improves forensic
analysis while leaving normal clip gestures, instrument authoring, routing,
or saving unusable has not completed the larger ambition.

### 1.2 The epistemic discipline

audec distinguishes three categories:

- **Signal:** something measured or cited from audio, such as a source frame
  range, spectral value, onset, or correlation.
- **Inference:** a hypothesis derived from evidence, such as a pulse
  alternative, recurring family, component, pitch trajectory, or proposed
  pattern.
- **Experience/authorship:** an attributed interpretation or deliberate edit,
  such as “cold,” “approaching,” a hand-authored note, or a chosen instrument
  identity.

Do not silently move an object from one category to another. In particular:

- A recurring hit family is not automatically a kick, snare, or hi-hat.
- An NMF component is not automatically a stem or instrument.
- A harmonic/percussive decomposition is not automatically isolated voices.
- A detected pitch is not proof of a particular synthesizer or performance.
- A classifier label is an attributed model claim, not source identity.
- A short source program is a useful simplicity prior, not proof of the
  producer's original method.
- A user may explicitly accept, name, edit, or promote a hypothesis. That
  transition must be represented by a command and provenance, not inferred
  from selection or display.

Honesty must be structural. Prefer a typed diagnostic, explicit unavailable
state, or visible temporary status over a silent fallback which looks
authoritative.

## 2. The product object model

Use these words consistently in UI text, code comments, documentation, and
design reasoning. Do not substitute implementation-domain words when the user
needs a product concept.

### Material

Imported, generated, or relinked audio with durable identity, availability,
fingerprint, and provenance. It lives in the Library. A Material is not an
arrangement clip and not an instrument.

### Sample

A whole material item or an exact half-open range of material used by an
instrument zone. A virtual slice normally cites frames of existing material;
it does not need to copy a file. A Sample is not a pad and not a global asset
name guessed from sound.

### Instrument

A playable sound source. Today this commonly means a sample kit, and later may
also mean a synth or plug-in. It owns playable addresses and output routing. It
is not a Pattern.

### Pad / zone

A playable address and one of its material mappings. A pad can be auditioned,
selected, routed, edited, and traced back to exact source evidence. It is a
sub-object of an Instrument, not a global material identity.

### Pattern

Reusable musical content: notes, steps, triggers, timing, expression, or a
retained generator term. A Pattern definition is not an arrangement placement.

### Pattern occurrence

One scheduled placement of a Pattern in the arrangement. Moving or trimming
an occurrence does not necessarily edit the Pattern definition. “Make unique”
is the explicit transition when shared content should fork.

### Audio clip

A ranged placement of Material on an arrangement Track. It may have trim,
slip, fades, stretch metadata, and placement time. It does not own the source
Material.

### Track

A musical lane containing occurrences or clips. A Track and a mixer Bus may be
bound, but they are different identities and must not masquerade as each other.

### Bus

A signal-routing and mixing point with level, pan, sends, inserts, latency,
and contributors. It is not a Track merely because a track routes to it.

### Automation lane

A reusable, typed parameter curve in musical time. It must identify the exact
parameter it controls. It is not an anonymous polyline.

### Finding

A measured result or set of alternatives about an Aspect. A Finding can be
inspected, auditioned, kept, dismissed, compared, or promoted. It is not yet
authored musical truth.

### Explanation

A retained construction claim with evidence and a renderable scope. It can be
heard alone or subtracted from source. It remains a claim even after it proves
musically useful.

### Comparison

A persistent, revision-pinned source/construction/residual/excess experiment.
It is not a temporary pane buffer and must name the exact project/source
revision to which it refers.

### Reading

A portable, attributed claim graph about matching source material. Readings
may contain competing hypotheses. Importing one does not silently merge or
accept its claims.

### Aspect

A compositional noun phrase selecting geometry and/or objects: time span,
frequency band, channel, family, project object, explanation, residual scope,
intersection, union, or complement. Actions consume Aspects. A viewport is not
automatically the selected Aspect.

### Source, Construction, Residual, Excess

These are signal layers over an Aspect, not duplicate object trees and not
separate transports. Construction, Residual, and Excess must name the
Explanation or Comparison to which they are relative.

## 3. The authority model

Most serious audec bugs are authority bugs: two places both believe they own
the project, selection, transport, viewport, analysis result, or audio output.
Preventing duplicate truth is more important than minimizing the number of
types.

### 3.1 One project authority

There is one authoritative aggregate `DawProject`, exposed through
`ProjectSession` and `ProjectController`. Durable mutations must become typed
commands applied through the project command path. Editors may hold immutable
snapshots and local gesture previews, but they do not become a second project.

A command envelope is more than an API wrapper. It is simultaneously:

- the aggregate undo/redo record;
- the autosave journal entry;
- the gesture-coalescing unit;
- the declaration of render invalidation;
- the future collaboration atom;
- the provenance of a deliberate project change.

Do not add a direct mutation path because it appears locally easier. Find the
existing command, extend the command algebra, or explicitly document why a
new command is required.

### 3.2 One render truth

Playback, comparison, and export must compile through the same deterministic
engine. “Bounce on play” is not a disposable placeholder. It gives audec a
valuable invariant: the project heard during playback is the project exported
offline.

The performance path is incremental bounce:

```text
project revision + dependencies
  -> immutable render plan
  -> content-addressed time/bus products
  -> coherent completed cohort
  -> one persistent audio host
```

Edits invalidate only affected products when dependency information is
precise. The scheduler renders loop/playhead work first. A partially complete
new cohort must not replace a complete old cohort. Do not solve a performance
problem by inventing a second mutable realtime project graph.

A bounded live preview path may exist for instrument gestures or continuous
controls, but committed sound must converge through the authoritative render
recipe.

### 3.3 One project transport

The application has one project playhead, play/pause state, loop, and audible
subject. Timeline-bearing panes may seek or request scoped audition through
that transport. They must not create private players.

There are two legitimate audible categories:

- **Timeline/scoped audition:** source, construction, residual, or another
  exact-span replacement on the shared project timeline. This can locate only
  when the control explicitly says it will.
- **Preview:** a short material, pad, piano note, medoid, or template played
  through the preview bus owned by the same audio host. A preview must not
  silently seek or change project transport.

Never add a pane-specific `AudioHost`, Rodio stream, CPAL stream, playhead, or
loop.

### 3.4 Selection, loop, playhead, and viewport are different

Keep these states separate:

- **Selection/Aspect:** what material or objects an action will operate on.
- **Loop:** the exact range which playback repeats.
- **Playhead/resume point:** where playback is or will resume.
- **Viewport:** what one pane currently displays.
- **Follow:** whether that viewport tracks the playhead.
- **Hover/edit cursor/drag preview:** local transient interaction state.

Required behavioral examples:

- Dragging a new selection must not seek, restart playback, or silently replace
  the current loop.
- `Command-L` sets the loop from the current selection and enables it.
- `L` toggles looping while preserving the disabled loop bounds.
- Manual pan or zoom disables follow for that pane only.
- Selecting an object may reveal it, but must not reset every pane's viewport.
- Sampling uses the visibly labeled active range. A new non-empty selection
  takes material precedence over a different older loop.

Conflating any two of these states recreates bugs already observed by the
user.

### 3.5 Views own presentation, not truth

A pane may own:

- viewport and zoom;
- local tool and hover state;
- gesture preview before commit;
- scroll position;
- focus memory;
- display recipe settings;
- a cancellable task handle and generation token.

A pane must not exclusively own:

- project objects;
- durable analysis claims;
- authoritative creative selection;
- project transport;
- an audio backend;
- a successful creation result;
- state that must survive save/reopen but is absent from the workspace
  document.

Moving or floating a pane must preserve its entity, target, local state,
selection relationship, in-flight task identity, and shared transport.

### 3.6 Every durable action must terminate visibly

The causal chain for a project action is:

```text
UI gesture
  -> typed intent/action
  -> validated command or prepared operation
  -> one authoritative project publication
  -> exact typed receipt/result
  -> global selection and/or reveal recommendation
  -> destination pane applies the target
  -> Inspector and Explorer reflect the same object
  -> render invalidation and audible consequence, if applicable
```

Trace all links before calling a workflow complete. A success toast, revision
number, or newly allocated ID is not a destination. The user must be able to
answer:

1. What did I make?
2. Where is it?
3. What is it routed through?
4. Can I hear it?
5. Can I edit it?
6. What evidence or source created it?
7. What should I do next?

Use existing `ObjectRef`, reveal requests/recommendations, receipt navigation,
and workspace targeting rather than inventing another completion abstraction.

## 4. The language model inside audec

audec does not embed Lua, Rhai, Python, or another mutable in-process scripting
runtime. This is a deliberate architectural choice, not a missing convenience.

The rule is:

> Anything that changes the project passes through the command algebra.
> Anything that computes is a pure, content-addressed term.

There are four related term languages:

1. **Commands are verbs.** Composition is a macro; inversion is undo; the
   journal is a durable program of edits.
2. **Aspects are nouns.** They identify what signal geometry, objects, or claim
   scopes an operation concerns.
3. **Pattern and curve expressions are generators.** They compile
   deterministically into ordinary production structures while retaining the
   source term as provenance.
4. **AIR queries are questions.** They inspect evidence and claim graphs
   without mutating the project.

Surface syntax is thin and late. The term type and evaluator are the semantic
authority. External general-purpose tools may speak a command/query protocol
across a process boundary. They do not receive mutable in-process access.

The flagship fusion is decompilation as program synthesis: analysis proposes
a short generator term, the ordinary DAW renders it, and residual/excess rank
how it fails. A generated Pattern retains its term origin. Hand grid edits mark
that origin as diverged; they never silently rewrite or clear the term.

Important existing pattern semantics are documented in `docs/LANGUAGES.md`:

- evaluator inputs include the real cycle index; otherwise alternation,
  `every`, `fast`, and `slow` can freeze at cycle zero;
- swing is a Pattern property and must not be baked into per-event
  micro-offsets, because the scheduler would apply it twice;
- rational timing remains exact as far as the sequencer representation allows;
- deterministic probability uses explicit seeds, never ambient randomness;
- a binding name such as `fam4` is not an instrument identity.

## 5. Read the repository in this order

Do not begin by reading random implementation files. Use this order:

1. `README.md` — current product surface, launch instructions, shortcuts, and
   honestly labeled limitations.
2. `docs/NEXT_CAMPAIGN.md` — the current quiescence checkpoint, remaining
   ambition, and next evidence-first priorities.
3. `docs/PRODUCT_INFORMATION_ARCHITECTURE.md` — product vocabulary, object
   homes, reveal contract, Explorer/Inspector roles, and action endings.
4. `docs/CREATIVE_WORKSPACE_IA.md` — detailed pane-by-pane authority audit and
   known cohesion failures. Treat its current-state table as historical where
   later commits have fixed a row; verify every claim in the live tree.
5. `docs/DAW_ARCHITECTURE.md` — the forward engine and domain contract.
6. `docs/SWARM_PLAN.md`, especially section 0 — integration discipline and
   historical workstream gates.
7. Read the task-specific normative document:
   - commands: `docs/COMMAND_ENVELOPE.md`
   - rendering: `docs/RENDER_TILES.md`
   - term languages: `docs/LANGUAGES.md`
   - coverage/comparisons: `docs/COVERAGE.md`
   - readings: `docs/READINGS.md`
   - workspace: `docs/WORKSPACE.md` and
     `docs/WORKSPACE_SESSION_LAYOUT.md`
   - interaction: `docs/DAW_UI_BLUEPRINT.md`
   - LMMS-like production scope: `docs/LMMS_PARITY_ROADMAP.md`
   - reverse program: `docs/DEPROJECTION_PROGRAM.md`
   - models: `docs/ML_MODELS.md`, `docs/BEAT_THIS_RTEN.md`, and
     `docs/ML_DSP_UPDATE_2026.md`
   - device/audio work: `docs/MIDI_INPUT.md`, `docs/PLUGIN_HOST.md`, and
     `docs/LINUX_SUPPORT.md`
   - ecosystem dependencies: `docs/RUST_ECOSYSTEM.md`
8. Only then inspect the relevant source modules and their tests.

Precedence when sources disagree:

1. The user's current request and observed behavior.
2. The live Rust tree and current tests for existing types and behavior.
3. `docs/NEXT_CAMPAIGN.md` for current campaign state.
4. A task-specific normative design for new behavior.
5. Product/architecture documents.
6. Old cycle plans, old audits, and commit hashes.

The tree wins for existing signatures. Do not copy a Rust signature from prose.
Use `rg` to find the actual definition at briefing time. A document may be
correct about intent while stale about which adapter is already connected.

## 6. Repository map

The crate is large. Search before adding a module. The following map names the
current homes of important concepts, but it does not replace reading the
actual files.

### Application and workspace

- `src/audec_app.rs`, `src/app_controller.rs`, `src/ui_platform.rs` — app and
  platform assembly.
- `src/ui.rs` — the large compatibility root and legacy Workbench integration.
  It is still load-bearing. Do not add new domain vocabulary or another
  authority here merely because the button is rendered here. Prefer a thin
  adapter into an existing service or extract a bounded presenter/controller.
- `src/ui_actions.rs`, `src/product_action_router.rs`, `src/product_input.rs`
  — typed actions and product routing.
- `src/workspace.rs`, `src/workspace_document.rs`, `src/workspace_items.rs`,
  `src/workspace_session_layout.rs`, `src/workspace_native_authority.rs`,
  `src/workspace_presenter.rs`, `src/workspace_ui.rs` — durable workspace,
  panes, docking/floating, native window state, and presentation.
- `src/explorer_model.rs`, `src/object_navigation.rs`,
  `src/project_reveal.rs`, `src/receipt_navigation.rs` — discoverability,
  product identity, and exact result reveal.
- `src/workspace_accessibility.rs`, `src/platform_semantics.rs` — keyboard,
  semantic, and platform behavior. A visible control is not complete if it
  cannot be focused or invoked without coordinate guessing.

### Project authority and persistence

- `src/project_session.rs` — UI-independent project session, revision stream,
  history, selection, and lifecycle-facing authority.
- `src/project_controller.rs`, `src/command.rs`, `src/command_journal.rs`,
  `src/command_record.rs`, `src/change_set.rs` — aggregate command application,
  validation, journaling, undo/redo, and invalidation.
- `src/daw_project.rs`, `src/live_project.rs` — aggregate domains, bindings,
  compatibility publication, and live project substrate.
- `src/project_session_lifecycle.rs`, `src/file_actions.rs`,
  `src/project_repository.rs`, `src/project_store.rs`, `src/project_io.rs`,
  `src/project_codecs.rs`, `src/project_format.rs` — save, open, recovery,
  atomic storage, durable codecs, and unknown-extension preservation.
- `src/persistence.rs` — older persistence stratum. Do not extend it without
  proving the newer project I/O path cannot own the behavior.

### Transport, rendering, and devices

- `src/audio.rs` — sample-exact transport primitives.
- `src/audio_host.rs` — backend-neutral persistent host contract.
- `src/project_audio_controller.rs` — the application authority connecting
  project publications to audio render cohorts.
- `src/transport_handoff_controller.rs` — exact state transfer during rare
  structural host replacement.
- `src/render_plan.rs`, `src/render_runtime.rs`, `src/render_service.rs`,
  `src/render_products.rs`, `src/render_tiles.rs`,
  `src/render_dependencies.rs`, `src/render_dependency_runtime.rs`,
  `src/render_validation.rs` — plans, scheduler, content-addressed products,
  incremental tiles, dependency invalidation, and validation.
- `src/daw_render.rs`, `src/daw_engine.rs`, `src/compiled_audio_graph.rs`,
  `src/graph_device_runtime.rs` — aggregate schedule compilation and DSP.
- `src/pane_audio.rs`, `src/pattern_audition_adapter.rs`,
  `src/pattern_audition_session.rs` — classification and routing of pane,
  pattern, and preview audio without private playback graphs.
- `src/device_service.rs`, `src/cpal_device_backend.rs` — device selection and
  the feature-gated direct CPAL backend.
- `src/export.rs` — deterministic WAV export from the same rendering truth.

### Production domains and editors

- `src/assets.rs`, `src/material_pool.rs`, `src/asset_view.rs` — Materials,
  source ranges, provenance, availability, browsing, and relinking.
- `src/sample_material.rs`, `src/sample_kit.rs`, `src/sample_runtime.rs`,
  `src/sample_actions.rs`, `src/sample_workflow.rs`,
  `src/workbench_sampling.rs`, `src/constructive.rs`,
  `src/constructive_controller.rs` — selection-to-sample/kit/pattern/occurrence
  preparation, exact PCM, atomic publication, and receipts.
- `src/instruments.rs`, `src/sampler_runtime.rs`, `src/sampler_pane.rs`,
  `src/sampler_view.rs`, `src/sampler_gate_lifecycle.rs` — sampler engine and
  Instrument/Pad/Zone editing and audition.
- `src/arrangement.rs`, `src/arrangement_actions.rs`,
  `src/arrangement_interaction.rs`, `src/arrangement_keyboard.rs`,
  `src/arrangement_surface.rs`, `src/arrangement_view.rs` — tracks, audio and
  pattern occurrences, direct manipulation, and arrangement UI.
- `src/sequencer.rs`, `src/sequencer_view.rs`,
  `src/sequencer_view/piano_workflow.rs`,
  `src/sequencer_view/step_workflow.rs`, `src/pattern_actions.rs`,
  `src/pattern_authoring.rs`, `src/pattern_controller.rs`,
  `src/pattern_runtime.rs`, `src/pattern_workflow.rs`,
  `src/pattern_use_graph.rs` — notes, steps, patterns, timing, editing, and
  occurrence relationships.
- `src/pattern_lang.rs`, `src/curve_lang.rs` — pure generator term languages.
- `src/automation.rs`, `src/mixer.rs`, `src/control_actions.rs`,
  `src/control_views.rs` — automation and mixer domains and editing surfaces.
- `src/plugin.rs`, `src/plugin_transport.rs`, `src/plugin_wire.rs`,
  `src/plugin_worker.rs`, `src/clap_adapter.rs` — isolated CLAP protocol,
  worker, state, and compatibility work.
- `src/midi_input.rs`, `src/musical_time_workflow.rs` — feature-gated MIDI and
  musical-time workflow.

### Reverse analysis, claims, and promotion

- `src/analysis.rs`, `src/pyramid.rs`, `src/spectral_tiles.rs`, `src/cqt.rs`,
  `src/hpss.rs`, `src/nmfd.rs`, `src/decomposition.rs`, `src/rhythm.rs`,
  `src/pitch.rs`, `src/loom.rs` — deterministic signal analysis and
  reconstruction engines. Read each module's negative claims before changing
  its output vocabulary.
- `src/analysis_product_runtime.rs`, `src/analysis_result_lifecycle.rs`,
  `src/task_coordinator.rs` — off-thread preparation, scheduling, cancellation,
  generation checks, and result publication. Large PCM hashes and DSP must not
  run synchronously on the GPUI thread.
- `src/ontology.rs`, `src/aspect.rs`, `src/explanation.rs`,
  `src/explanation_adapters.rs`, `src/generative_ontology.rs`,
  `src/model_claim.rs`, `src/inference_recipe.rs` — AIR, geometry, evidence,
  explanations, source programs, and attributed claims.
- `src/reconstruction.rs`, `src/reconstruction_apply.rs`,
  `src/deprojection_program.rs`, `src/deprojection_expression.rs`,
  `src/deprojection_evaluation.rs`, `src/deprojection_execution.rs`,
  `src/deprojection_promotion.rs`, `src/deprojection_workspace_bridge.rs`,
  `src/generative_lowering.rs` — proposals, evaluation, atomic lowering into
  ordinary production objects, and reveal.
- `src/rhythm_explanation.rs`, `src/rhythm_promotion.rs`,
  `src/rhythm_promotion_chooser.rs`, `src/beat_this_deprojection.rs`,
  `src/beat_this_deprojection_controller.rs` — rhythm-specific reverse paths.
- `src/reverse_surface.rs`, `src/reverse_surface_adapter.rs`,
  `src/reverse_surface_view.rs`, `src/explanation_pane_model.rs`,
  `src/explanation_workbench_view.rs`, `src/reverse_navigation.rs` — Finding,
  Explanation, Compare, and reverse-workbench UI.
- `src/comparison.rs`, `src/comparison_controller.rs`,
  `src/comparison_runtime.rs`, `src/comparison_runtime/executor.rs`,
  `src/coverage.rs` — persistent comparison semantics, aligned renders,
  residual/excess, and explained-energy coverage.
- `src/interpretation.rs`, `src/interpretation_navigation.rs`,
  `src/reading.rs`, `src/reading_codec.rs`, `src/reading_workflow.rs`,
  `src/project_reading_query_session.rs`, `src/reading_query_protocol.rs`,
  `src/reading_query_workbench.rs`, `src/reading_query_view.rs`,
  `src/reading_effect_bridge.rs`, `src/air_query.rs` — portable readings,
  queries, effects, and navigation.

### Content, models, workers, and streaming

- `src/content_identity.rs`, `src/content_store.rs`,
  `src/artifact_catalog.rs`, `src/artifact_comparison_hydration.rs`,
  `src/artifact_promotion_bridge.rs` — content-addressed artifacts and their
  links into comparison/promotion.
- `src/model_registry.rs`, `src/model_store.rs`, `src/model_supervisor.rs`,
  `src/model_task_service.rs`, `src/model_worker.rs`, `src/model_wire.rs`,
  `src/worker_broker.rs`, `src/worker_runtime.rs` — verified local model
  artifacts, process isolation, task ownership, protocol, and worker lifecycle.
- `src/beat_this.rs` and `src/bin/audec-beat-this-worker.rs` — the first
  concrete rhythm-model seam, feature-gated by `beat-this-rten-worker`.
- `src/streaming_media.rs`, `src/media_resolver.rs` — media access and explicit
  decode/materialization boundaries.

If you think a new module is needed, first run:

```sh
rg -n "the concept or type name" src docs
rg --files src | sort
```

Prefer deleting a duplicate authority or connecting an existing terminal edge
over inventing a fifth representation of the same idea.

## 7. Current checkpoint: what is real and what remains

At checkpoint `e1e6bdf`, the primary library gate passed 1,290 tests, with zero
failures and two explicit ignores. This count will change. Do not edit a test
count in documentation unless you have just run the full applicable suite.

Substantial integrated foundations already exist:

- the owned `emberian/gpui-toolkit` GPUI universe and Guise workspace;
- dynamic durable docked and floating panes;
- typed action/menu/command-palette registration;
- one project session and command-owned aggregate;
- one selection and transport authority, with timeline versus preview audio
  distinguished;
- one backend-neutral persistent host, default Rodio and optional direct CPAL;
- coherent incremental render products and content-addressed render tiles;
- native Save, Save As, Open, autosave recovery, and WAV export;
- exact Material/Sample/Instrument/Pad/Pattern/occurrence construction with
  durable receipts and one-step undo;
- pattern mini-notation and term provenance;
- Finding/Explanation/Comparison/Reading models and shared
  source/construction/residual audition paths;
- off-thread content preparation before Components, Rhythm, HPSS, and Loom
  scheduler admission;
- isolated plug-in and model worker protocols;
- Ubuntu compile and virtual-X11 smoke coverage.

Do not mistake that substrate for a finished product. The highest-value
remaining work includes:

- a complete real-material first-five-minute musician journey with desktop
  evidence rather than only model tests;
- further removal of authority and adapter logic from the very large `ui.rs`;
- reliable exact-result selection across Explorer, Inspector, and every
  editor;
- deeper ordinary DAW gestures and coherence: clip/pattern manipulation,
  Instrument authoring, pattern-from-pads, mixer routes and meters,
  automation, numeric entry, keyboard coverage, MIDI/device setup, and a
  deliberate real plug-in matrix;
- profiling and removing remaining pane activation/viewport hitches,
  especially Loom viewport reconstruction;
- generalizing explain-as-program beyond rhythm/Loom into pitch,
  modulation, curves, masks, texture, coverage, excess, and competing models;
- verified user-facing model installation and at least one real separation
  adapter through the existing broker;
- physical macOS device recovery, Linux X11/Wayland, portals, audio devices,
  and packaging evidence;
- recording, latency calibration, and robust device selection;
- optional multiplayer as an adapter over commands/readings, never another
  project authority.

The current supported alpha runtime target is macOS. Linux is a developer
preview. Do not claim physical Linux support because CI compiled or opened a
window under virtual X11.

## 8. How to choose and scope work

Prefer work in this order unless the user asks for something else:

1. A user-observed crash, hang, false success, data-loss risk, transport error,
   or action which creates an unreachable object.
2. A missing integration edge between already-real foundations.
3. A responsiveness problem caused by work on the GPUI thread.
4. A high-frequency production gesture which shortens intent-to-sound.
5. A reverse-to-production path which creates editable ordinary objects and
   closes the audible residual loop.
6. New foundational capability which has a named end-to-end consumer.
7. Refactoring which deletes duplicate authority or makes the above work
   cheaper.

Avoid broad vocabulary waves which create types without making a user journey
more complete. However, do not avoid architecture when architecture is the
reason the journey cannot become coherent. The goal is large, connected
bricks followed by convergence—not many decorative modules and not one giant
rewrite.

For each candidate task, answer these questions before coding:

1. What exact user gesture begins the workflow?
2. What Aspect and project revision does it consume?
3. Is preparation expensive? If yes, what immutable snapshot is prepared off
   the UI thread?
4. What command commits the result?
5. What durable identities are created or changed?
6. What receipt names them?
7. What should become globally selected?
8. Which pane should reveal the result, and how does it acknowledge the
   target?
9. What render dependencies are invalidated?
10. What will the user hear, and through which shared audio category?
11. What happens on undo, redo, save, reopen, and export?
12. What evidence/provenance survives?
13. What stale-revision, cancellation, missing-media, unavailable-model, or
    device-failure diagnostics are visible?

If you cannot answer a question, search the tree before creating a solution.

## 9. Exact implementation procedure

Follow this sequence for an ordinary implementation task.

### Step 1: Establish repository state

Run:

```sh
cd /Users/ember/dev/audec
git status --short
git log -5 --oneline --decorate
```

Read any existing diffs in files you may touch:

```sh
git diff -- path/to/file.rs
git diff --cached -- path/to/file.rs
```

Do not assume an uncommitted line is yours.

### Step 2: Locate the existing causal chain

Use `rg`, not a broad manual scan. Examples:

```sh
rg -n "SamplePublishedResult|ConstructivePublication|SampleResultFocus" src
rg -n "ProjectAudioController|ScopedAudition|Preview" src
rg -n "RevealRequest|RevealRecommendation|ObjectRef" src
rg -n "CommandEnvelope|DomainCommand|apply_envelope" src
rg -n "TODO|todo!\(|not connected|unavailable" src docs
```

Read the actual struct, enum, constructor, call sites, module header, and tests.
Do not infer a signature from a similarly named type.

### Step 3: Write a small dependency plan

The plan should identify:

- files to change;
- shared types affected;
- who owns each mutation;
- UI-thread versus worker-thread work;
- success receipt and reveal destination;
- focused tests;
- full integration and manual gates.

If multiple agents are working, assign single-writer files explicitly. In
particular, `src/ui.rs`, `src/live_project.rs`, and `src/daw_project.rs` should
not receive concurrent overlapping edits. Agents may defer compilation to a
convergence wave only when the coordinator explicitly plans that wave; they
must still report exact changed files and unresolved interfaces.

### Step 4: Implement through existing authorities

Prefer this shape for expensive product actions:

```text
capture immutable, revision-pinned input
  -> prepare/hash/analyze off the GPUI thread
  -> return owner + generation + receipt candidate + diagnostics
  -> validate revision/generation briefly on the authority thread
  -> apply one command envelope
  -> publish one immutable snapshot
  -> reveal exact result
```

Do not hold a project mutex during large DSP, hashing, file decode, model
inference, or render work. Do not let an obsolete completion replace newer
state. Check owner, generation, and source/project revision at admission.

Use deterministic seeds. Validate configurations at their boundary. Quarantine
non-finite PCM. Use `total_cmp` and an explicit stable tie-breaker when sorting
floats. Use typed IDs rather than an array index, UI row, or display name.

### Step 5: Format and inspect the diff

Run:

```sh
cargo fmt --all
git diff --check
git status --short
git diff --stat
git diff -- path/to/each/changed/file
```

Read the diff. Specifically look for:

- unrelated formatting churn;
- accidental generated files;
- a second authority or audio path;
- an error converted into a silent fallback;
- demo data used where project data is required;
- long work newly placed in a GPUI callback;
- an object created without selection/reveal;
- a UI affordance whose callback is absent;
- documentation claiming more than the test proves.

### Step 6: Run proportional tests

During iteration, use focused filters. Examples:

```sh
cargo test --lib project_session::
cargo test --lib render_tiles::
cargo test --lib sample_workflow::
cargo test --lib engine_regression::
```

Before landing a shared-struct or cross-cutting change, run:

```sh
cargo check --lib
cargo test --lib
cargo build --bin audec
```

For a broad change or release checkpoint, also run:

```sh
cargo test
```

For direct-device work:

```sh
cargo check --lib --features cpal-device
```

For MIDI work:

```sh
cargo check --lib --features midi-input
```

For Beat This worker work:

```sh
cargo check --features beat-this-rten-worker --bin audec-beat-this-worker
```

Do not relax determinism, allocation-safety, residual/null, identity,
round-trip, or stale-generation tests merely to make a change green. A test
which only restates its own fixture is not evidence. Read the assertion and
explain what failure it would catch.

### Step 7: Perform the relevant live workflow

Compilation is not proof of UI usability. For a UI or audio workflow, run the
current debug binary on real material after verifying the path:

```sh
rg --files "/Users/ember/Desktop/The Knife - Silent Shout" | rg "Like a Pen.*\\.flac$"
cargo build --bin audec
./target/debug/audec "/the/exact/path/returned/above.flac"
```

If another audec instance is running, do not kill it blindly. It may contain
unsaved user state. Ask or inspect before replacing it.

Record what you actually observed. “The app compiled” is not desktop evidence.
If you cannot operate or inspect the GUI, say the live gate is unverified.

### Step 8: Commit and push a coherent checkpoint

Stage named files only:

```sh
git add src/exact_file.rs src/another_exact_file.rs docs/exact_doc.md
git diff --cached --check
git diff --cached --stat
git commit -m "Imperative summary of the completed outcome"
git push origin main
```

Never use `git add -A` or `git add .` in this repository. Do not include user
changes merely because they are present. Small commits are welcome when they
represent coherent bricks. Untested commits are acceptable only when the user
or coordinator explicitly wants a rapid brick wave and a named convergence
wave will test them; label the unverified state honestly in the handoff.

### Step 9: Hand off exact evidence

Report:

- commit hash and subject;
- whether it was pushed to `origin/main`;
- exact files changed;
- focused tests and counts;
- full test/build result and counts;
- live workflow observed;
- anything not verified;
- known follow-up integration edges;
- whether the working tree is clean.

Do not write “done” without those facts.

## 10. Manual musician acceptance script

Use this script for convergence work. It is deliberately redundant: it catches
authority splits which isolated tests miss.

### Launch and workspace

1. Launch with the real *Like a Pen* FLAC.
2. Confirm the app remains responsive while initial analysis prepares.
3. Confirm the macOS window controls do not overlap a tab.
4. Resize the window until sidebars overflow; every intended long surface must
   scroll without hiding controls.
5. Open several panes. Reorder, split, float, and dock one back.
6. Confirm the moved pane retains its target, viewport, follow state, and any
   in-flight or completed analysis.
7. Quit normally. A dirty project must offer Save/Discard/Cancel; a clean
   project must quit. The app must not become impossible to quit.

### Timeline, selection, and loop

1. Start playback.
2. Drag a selection while playback continues.
3. Confirm the drag does not seek or resume from an older loop start.
4. Press `Command-L`. Confirm the loop adopts the exact selection and is on.
5. Press `L` twice. Confirm disabling preserves the bounds and reenabling uses
   the same bounds.
6. Drag a different selection. Confirm the old loop remains audible until
   explicitly replaced and the new selection is visibly the active material
   range.
7. Pan/zoom independently. Confirm this disables Follow without changing the
   playhead or loop.
8. Reenable Follow. Confirm the visible range now follows playback.
9. Zoom the log-frequency view. Confirm it recomputes at viewport-native
   resolution rather than magnifying a blurred whole-song bitmap.

### Sample, Slice, and Beat

1. Make a clearly visible non-empty selection.
2. Choose **Sample**. Confirm an exact ranged Sample and Instrument/Pad are
   created, the Instrument surface opens, the new Pad is selected, and the
   Inspector shows exact source frames, material, provenance, and route.
3. Undo once. Confirm all pieces of that construction disappear together.
4. Redo once. Confirm the same identities and audible result return.
5. Choose **Slice** on a range. Confirm an Instrument with multiple Pads is
   revealed and each pad can preview without moving the project playhead.
6. From the Instrument, continue to a Pattern without returning to an
   unrelated source footer. If this action is not yet present, record it as a
   concrete remaining product gap rather than pretending Slice completed a
   beat workflow.
7. Choose **Beat**. Confirm Samples, Pads, Pattern, Pattern occurrence, track,
   and mixer route are created atomically, with the intended destination
   selected.
8. Edit the step Pattern while the loop plays. Confirm the next coherent
   render contains the edit and no old render replaces a newer revision.
9. Confirm there are no unroutable sequencer diagnostics on the happy path.

### Production panes

1. In Arrange, move, trim, split, duplicate, and delete a typed clip or Pattern
   occurrence. Confirm each created result becomes the active inspectable
   object.
2. Distinguish editing a Pattern definition from moving its occurrence.
3. In Pattern, create or duplicate content and confirm Explorer/Inspector show
   the exact target.
4. Audition a Pad and a piano note. Each must use the shared preview route or
   be disabled with a visible reason; a decorative play control is a bug.
5. Create a mixer Bus or routing edge. Confirm the new Bus is selected and its
   contributors/route are inspectable.
6. Create an Automation lane. Confirm the new lane, not an older compatible
   lane, becomes the editing target.

### Reverse panes

1. Open Components, Rhythm, HPSS, and Loom individually. Pane activation must
   not synchronously hash/scan the whole song on the UI thread.
2. Start a task, change its input or close/reopen the pane, and confirm stale
   completion cannot publish over the newer generation.
3. In HPSS, use Source/Harmonic/Transient/Residual. Confirm all are scoped
   layers on the shared transport, not private clocks.
4. In Loom, edit an event/template construction and hear Source,
   Construction, and Residual on the same timeline.
5. Apply/promote a proposal. Confirm it creates ordinary Materials,
   Instruments, Patterns, occurrences, automation, or routes through one
   project command and reveals them.
6. Undo once. Confirm the construction objects, evidence backlinks, and
   audible result revert together.
7. Retain a Finding or Comparison, close its pane, reopen/reveal it, and
   confirm it was not merely a pane-local buffer.
8. Import or open a Reading only as attributed competing claims. Confirm no
   imported hypothesis becomes accepted truth automatically.

### Persistence and export

1. Use Save As to create a `.audec` project.
2. Change the workspace layout and edit a musical object.
3. Quit and reopen the saved project.
4. Confirm durable identities, objects, routes, generated PCM, provenance,
   target panes, viewports, follow state, and native window placement restore.
5. Export WAV from the same frozen revision heard in playback.
6. Compare or null the export against the authoritative rendered product when
   the workflow provides that evidence.
7. Simulate or use an interrupted autosave only in a controlled test and
   confirm recovery is explicitly labeled.

## 11. Performance and concurrency rules

The GPUI thread may capture input, mutate short presentation state, submit a
command, admit a prepared result, and render UI. It must not perform a
whole-song hash, spectral scan, resample, decode, model inference, project
render, or large serialization operation synchronously.

For content-addressed work, distinguish:

- **Preparation:** potentially expensive hashing, canonicalization, source
  receipt construction, DSP, or decode. Run this off-thread on immutable input.
- **Admission:** short authority-thread validation that owner, generation,
  project revision, input identity, and cancellation state remain current.
- **Publication:** one immutable result and typed diagnostic/reveal outcome.

Cancellation must be observable at bounded intervals. An old task may finish
computing, but it must fail admission if its generation is stale. Do not rely
on “it usually finishes in order.”

The audio pull/callback path must not allocate, block, lock a project graph,
perform I/O, or wait for a render. Starvation is an explicit counted state,
not permission to block the callback.

Use cache leases or pinned cohorts so the currently audible product is not
evicted while transport rolls. Invalidation may be conservatively broad, but
never incorrectly narrow.

## 12. Dependency and platform rules

The current crate intentionally owns one compatible GPUI/WGPU universe through
the pinned `emberian/gpui-toolkit` fork and a matching pinned Guise fork. Before
changing any GPUI, WGPU, Vello, Rodio, CPAL, Symphonia, MIDI, or plug-in
dependency:

1. Read `Cargo.toml` comments around the dependency.
2. Run `cargo tree -d` and inspect duplicate platform/DSP families.
3. Check the pinned fork and revision; do not replace it casually with a
   crates.io line which creates type-incompatible universes.
4. Build default features and each relevant opt-in feature.
5. Update `docs/RUST_ECOSYSTEM.md` or the relevant platform document if the
   decision changes.

Current important features are:

- `beat-this-rten-worker`
- `midi-input`
- `cpal-device`
- `ui-vello`

The default backend is currently Rodio. `cpal-device` replaces it behind the
same application host contract; it must not open alongside it. Vello remains
surface-specific and opt-in; do not introduce a second renderer just to adopt
a widget.

GPL dependencies are acceptable to the project owner, but provenance,
license, maintenance, dependency weight, and architectural fit still require
review. Prefer mature community crates for solved infrastructure. Fork into an
owned repository only when audec genuinely needs to control or modify the
code, and pin an audited revision.

## 13. Common failure modes: do not repeat these

### False completion

Bad:

```text
button -> command -> revision label
```

Good:

```text
button -> command -> typed created identities -> select -> reveal -> inspect
       -> hear/edit -> undo/save/reopen
```

### Demo-backed success

Do not use demo patterns, placeholder buses, default instruments, or generated
fallback data when the UI claims to show the live project. A demo can be an
explicit example, but it does not satisfy a real-project gate.

### Empty engine configuration

Do not use `DawEngineConfig::default()` in a path which needs project
Instruments or sample bindings. Build the engine configuration from the
authoritative project bindings, materialized PCM, Instruments, and routes.
Unroutable events must produce visible diagnostics, not silence presented as
success.

### Private audio in a pane

Do not create a separate player for HPSS, Loom, a Pattern, or Compare. Use
scoped timeline audition or preview through the shared host, depending on the
semantic category.

### Viewport equals selection

Do not assume “what is visible” is “what the action will modify.” Show the
active range explicitly. If an action intentionally uses the viewport, label
it `Analyze view` or equivalent.

### Click equals seek and drag equals loop

Pointer press/drag/release must be modeled explicitly. A selection drag must
not invoke the click-seek behavior on release. A selection does not become a
loop until the user performs the loop action.

### Fit on publication

Do not reset a pane to the song start or fit-whole every time a project
snapshot arrives. Project publication changes data; local viewport/follow
state remains local and durable unless a typed reveal requests navigation.

### Blurred spectrum

Do not scale a fixed whole-song bitmap to satisfy a zoom. Query retained
numeric/spectral data for the visible sample range and current pixel width.

### Pane-local durable analysis

Do not let an auditionable or editable reverse result vanish when its pane
closes. Either label it temporary and offer Keep/Apply/Dismiss, or publish a
Finding/Explanation/Comparison with typed identity.

### Heavy work on UI activation

Do not hash large PCM buffers or run analysis synchronously because a tab was
clicked. Use the preparation/admission boundary in
`analysis_result_lifecycle.rs` and related runtime services.

### Local snapshot mutation

Do not mutate a cloned Asset registry, Sequencer, Mixer, or Automation graph
and display success. The next project publication will overwrite it. Submit a
project command, then update from the authoritative publication.

### Identity by name, index, or classifier

Use typed durable IDs and explicit bindings. Display names are mutable. Array
indexes are presentation accidents. Classifier labels are claims.

### Silent recovery

Do not silently substitute a predecessor, old render, missing model, default
route, or absent asset. Report the exact fallback or refusal and keep the
project recoverable.

### In-process scripting

Do not embed a mutable interpreter. Extend the typed term language or external
command/query protocol.

### Worktree and target sprawl

Do not create a worktree per small task. Reuse the canonical checkout. Use
`cargo clean` only when target bloat materially matters, after confirming no
other build is active; it deletes all compiled artifacts and forces a complete
rebuild. Never recursively delete a broad temp or home directory.

## 14. Debugging hangs and crashes

When the app hangs or crashes, preserve evidence before restarting it.

1. Record the exact gesture and visible state immediately before failure.
2. Record whether audio continues, whether the window repaints, and whether
   only one pane or the whole process is unresponsive.
3. Find the process without killing it:

   ```sh
   pgrep -lf '/target/(debug|release)/audec|Audec.app'
   ```

4. On macOS, capture a sample of the exact PID:

   ```sh
   sample THE_PID 10 -file /tmp/audec-hang-sample.txt
   ```

5. Read the sample for main-thread stacks, mutex waits, hashing, DSP, decode,
   or render work.
6. Inspect app logs and the relevant task generation/owner state.
7. Only then terminate the exact PID if the user has no unsaved work or has
   authorized termination.
8. Turn the failure into the narrowest non-vacuous regression test possible.

For a pane-activation hang, first suspect synchronous content hashing,
whole-song analysis, viewport reconstruction, a lock held across DSP, or a
completion callback waiting on the same authority it must reenter.

For a transport bug, log selection, loop bounds/enabled state, playhead,
resume point, mode, pending seek generation, host generation, and audible
subject separately. Do not print one ambiguous “position.”

## 15. When work is blocked

Do not convert “uncertain” into either a guess or a permanent blocker.

1. Re-read the actual type and every call site.
2. Search for an existing service, receipt, adapter, or test fixture.
3. Run a focused diagnostic or write a small model test.
4. State the exact missing fact, authority, hardware, model artifact, or user
   decision.
5. Continue any independent, safe work which does not depend on that fact.
6. If the same blocker truly prevents all progress, report it with commands,
   output, changed files, and the smallest decision needed.

Never weaken validation, remove a diagnostic, invent source identity, or
silently change product semantics to get around a blocker.

## 16. Definition of done

A change is complete only to the level its claim requires.

### Core/model complete

- Types and logic exist in the correct authority.
- Focused tests exercise meaningful failure cases.
- Whole-tree compilation passes if shared types changed.
- No UI claim is made.

### UI-connected complete

- The live product action reaches the core through typed intent.
- Success returns exact identities and reveals/selects its destination.
- Errors and unavailable states are visible.
- Keyboard/focus/scroll behavior is coherent.
- A manual desktop workflow has been observed, or explicitly remains
  unverified.

### Audible complete

- The authoritative project renderer contains the result.
- The shared host plays the intended revision.
- Diagnostics are empty on the happy path.
- Undo/redo and coherent render replacement work.
- Export uses the same render truth.

### Durable complete

- Save/reopen preserves identity, content, provenance, layout, and bindings.
- Autosave/recovery behavior is defined.
- Unknown extension data remains preserved where the format promises it.

### Product-gate complete

- A real musician journey reaches the outcome without hidden debug controls,
  demo data, coordinate guessing, or a stale completion banner.
- The result is discoverable, inspectable, audible/editable, and reversible.
- Desktop evidence exists on the claimed platforms.

Choose the smallest honest completion label. “Backend exists” is valuable, but
it is not “the feature works in the app.”

## 17. Handoff template

Use this exact structure when handing work to another agent or the user:

```text
Outcome
- One sentence describing the user-visible or architectural result.

Conceptual effect
- Which authority became clearer or which product loop became more complete.
- Which audec invariant constrained the implementation.

Commits
- <hash> <subject> (pushed/not pushed)

Files
- <exact paths and why each changed>

Verification
- <focused command>: <result/count>
- cargo check --lib: <result>
- cargo test --lib: <result/count>
- cargo build --bin audec: <result>
- live workflow: <exact observation or NOT VERIFIED>

Known limitations
- <specific remaining edges; no vague “polish remains”>

Repository state
- branch: <name>
- HEAD: <hash>
- git status --short: <clean or exact entries>
```

## 18. The north-star test

For every mark, pane, object, command, or model result, be able to answer:

```text
What evidence is this?
How was it derived?
What refuses to be claimed?
Can I hear it?
What happens if I edit it?
What term denotes it?
What command produced it?
Where does it live?
How do I reveal it again?
Does undo/save/reopen preserve its truth?
```

For every production gesture, also answer:

```text
How quickly can the musician get from intent to sound?
Is the result an ordinary editable project object?
Is the audible revision the exportable revision?
Does provenance remain available without becoming a tax?
```

The largest vision of audec is not “a DAW with analysis panes.” It is a
coherent musical environment in which finished sound can be investigated as
evidence, proposed as executable structure, edited as music, rendered through
one truth, tested by its audible failure, and exchanged as an attributed
reading. Build ordinary production quality and epistemic honesty together.
