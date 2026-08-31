# Product information architecture: one musical desk, one claim graph

Status: product/interaction architecture proposal grounded in the live tree at
`97b0b6c` and the subsequent pane-session convergence work, 2026-08-31.

This is deliberately a product document, not a restatement of Rust module
boundaries. It names the things a musician encounters, states where they live,
and defines what happens after every meaningful action. The current code has
strong domain boundaries and a growing dynamic workspace; the application
still exposes several of those boundaries as separate destinations. This
document is the contract for turning them into an excellent electronic-music
DAW *and* an honest reverse-production environment.

The central rule is simple:

> A project is one playable construction and one claim graph about source
> material. Every object has a home, an inspector, an audible consequence or
> an explicit reason it has none, and a stable way to reveal it after it is
> created.

The reverse direction is credible only because the forward direction is a
real instrument. Conversely, the production surface should not force a user
to forget where a sound came from. Provenance is available on demand; it is
not a tax on making a beat.

## 1. Product diagnosis

### 1.1 The live application is capable but not yet legible as a desk

The current tree already has substantial, real substrate:

- opening source material registers a media-pool asset and initializes one
  `LiveProject` with arrangement, sequencer, mixer, assets, bindings, sample
  kits, transport, render, undo, and persistence;
- a source-range action can create non-copying virtual slices, materialize
  PCM for rendering, create a sample kit and pads, route it to a mixer bus,
  bind it to sequencer targets, create a pattern, and place a pattern clip in
  the arrangement in one constructive transaction;
- the sampler can audition pads, edit zones, change a kit output, and retain
  exact range/provenance; the step/piano editor has real patterns and a
  retained expression origin; arrangement, automation, mixer, and media pool
  are real editors rather than mockups;
- rhythm promotion and reconstruction application deliberately create
  anonymous editable material with typed evidence and diagnostics rather than
  silently naming hypotheses as instruments;
- Loom and HPSS already prove the core reverse interaction: source,
  construction, and residual can be heard separately.

Those truths are obscured by an interaction topology inherited from the
analyzer/workbench era. The default Overview is simultaneously transport,
source atlas, source selection, action launcher, layer list, and a small
inspector. It opens several editor or lens windows as side effects. The
dynamic workspace can host target-bearing panes, but the product vocabulary
still says "open editor", "media pool", "components", and "decompose selected
span" rather than describing one navigable project.

The result is not that the user lacks capabilities. The user lacks a reliable
answer to: **what did I just make, where is it, what is it routed through,
what should I do next, and how do I get back to it?**

### 1.2 Why “extract to instrument” appears to go nowhere

This is a concrete current-state failure, not a hypothetical usability risk.

1. The Overview rail calls source-range commands labelled `One-shot`, `Chop
   ×8`, and `Make 1-bar beat`. `One-shot` intentionally creates only a kit,
   pad(s), and exact source material; it does not create a pattern or an
   arrangement occurrence. That is correct semantics but invisible product
   feedback.
2. `Chop` likewise creates a kit/pads unless the command is specifically a
   make-beat plan. Its successful location is a sampler kit, not the source
   waveform and not necessarily the arrangement.
3. Constructive execution returns exact durable identities:
   `ConstructivePublication { kit, pad, pattern, arrangement_clip, focus }`.
   `SamplePublishedResult` retains the same kit/pad/pattern focus. This is an
   unusually good backend handoff.
4. The Overview-originated actions currently reduce that result to a short
   status string such as a revision number. They have no originating sampler
   view to retarget and no application-level navigator to reveal the kit,
   pad, pattern, or clip.
5. Browser and sampler views support a `SampleFocusCallback`, but the live
   dynamic factories do not install one. A successful `SampleResultFocus` is
   consequently dropped. A user sees that a command did not fail yet sees no
   instrument, pattern, clip, or highlighted pad.
6. The phrase "selection to instrument" wrongly makes a normal musician
   action feel like an opaque conversion. It also hides the important choice:
   make a reusable sample, slice a kit, make a pattern, or make an arranged
   beat are different outcomes.

The fix is not another toast. It is a universal **Reveal Object** contract
defined in section 5.

### 1.3 Current-state truth: what should not be papered over

The following are product facts to preserve while reworking the surface:

| Current fact | Product implication |
| --- | --- |
| A media asset has fingerprint, location, availability, usages, and provenance. | It belongs in a Library and can be inspected or relinked; it is not merely a waveform clip. |
| A virtual slice points to an exact half-open range of an asset. | It is a material reference, not a duplicate audio file and not an unnamed mutable sample. |
| A sample kit has pads, zones, output route, revision, and pad keyboard mapping. | It is an Instrument. A pad/zone is a playable sub-object of that instrument. |
| A pattern definition is reusable musical content. A pattern clip is one scheduled occurrence. | "Pattern" and "clip" must never be used interchangeably in labels or navigation. |
| Arrangement tracks and mixer buses are different but bound. | The Track view shows musical placement; the Mixer shows signal routing. Neither should masquerade as the other. |
| Rhythm/reconstruction alternatives remain hypotheses until a user applies one. | A finding may be previewed and promoted but must not appear as a finished instrument before acceptance. |
| Loom/HPSS results are currently lens-local; comparisons/readings have dedicated models but incomplete app reachability. | The surface must distinguish a temporary investigation from a saved comparison or portable reading. |
| Project IDs live in several domains by design. | UI must use product references/labels and deep links, never raw numeric IDs as its normal identity system. |

## 2. The user-facing object model

The product should use the following vocabulary consistently in menus, titles,
breadcrumbs, search, activity, and inspectors. Parentheses identify existing
technical representations, not labels that should leak into the UI.

| Product object | What it is | Lives in | Primary actions | What it is not |
| --- | --- | --- | --- | --- |
| **Material** (asset) | Imported, generated, or relinked audio with identity and provenance. | Library › Materials. | audition, tag, rename display name, relink, reveal uses, make sample. | An arrangement clip or an instrument. |
| **Sample** (source material reference) | A whole material item or exact ranged slice used by a zone. | Library › Samples; also shown under an Instrument. | audition, trim non-destructively, duplicate-as-new range, reveal source, reveal uses. | A copied file by default. |
| **Instrument** (sample kit today; synth/plugin later) | A playable sound source with output routing and controls. | Project › Instruments. | play pads, map material, rename, choose output, duplicate, open instrument editor. | A pattern or an audio track. |
| **Pad / zone** | A playable address and one of its material mappings. | Inside an Instrument. | audition, select, trim/loop/envelope, replace/map material, reveal source/evidence. | A global asset identity. |
| **Pattern** (pattern definition) | Reusable note/step/generator content. | Project › Patterns. | edit in piano/step editor, rename, duplicate, change origin, reveal placements. | A scheduled instance. |
| **Pattern occurrence** (pattern clip) | One placement of a pattern on an arrangement track. | Song › Arrangement. | move/trim/loop/mute, open pattern, make unique, reveal pattern. | The pattern definition itself. |
| **Audio clip** | One ranged placement of material on a track. | Song › Arrangement. | move/trim/slip/fade, open material, reveal source/provenance. | Material ownership. |
| **Track** | A musical lane of placements and controls. | Song › Arrangement. | arrange, rename, color, arm/mute/solo, reveal route. | A mixer bus, though it has one. |
| **Bus** | Signal routing/mix point. | Song › Mixer / Inspector route section. | level/pan/inserts/sends, rename, reveal contributors. | A track identity. |
| **Automation lane** | A reusable parameter curve addressed to one parameter. | Song › Automation and Inspector. | draw/term-edit, retarget, show on arrangement, reveal parameter. | A generic unlabeled curve. |
| **Finding** | A measurement or a set of alternatives about selected material. | Investigate › Findings. | inspect evidence, audition, compare, preview, promote/apply, dismiss. | Authored musical structure. |
| **Explanation** | A selected or retained construction claim with evidence and renderable scope. | Investigate › Explanations. | inspect evidence, hear construction/residual, compare, edit construction, retain in reading. | Source identity or certainty. |
| **Comparison** | A persistent, revision-pinned source/construction/residual experiment. | Investigate › Comparisons. | A/B/R audition, see coverage/excess, refresh, export, reveal scope. | A mere lens buffer. |
| **Reading** | A portable attributed claim graph about matching source material. | Readings. | verify, inspect, audition derived attachments, diff, import as alternatives, export. | A backup, stem pack, or authority claim. |

### 2.1 One relationship model, not a pile of panes

```text
Library Material ── exact range ──> Sample ── mapped by ──> Instrument / Pad
       │                                                        │
       └── placed as Audio Clip ──> Track ── routed to ──> Bus ─┤
                                                                │
Pattern ── placed as Pattern occurrence ───────────────────────┘
   ▲
   └── authored directly | generated by a term | promoted from a Finding

Source material ── evidence ──> Finding / alternative ── selected/apply ──>
                           Explanation / editable construction
                                     │
                         source + construction + residual
                                     │
                                Comparison ──> Reading
```

Every arrow is revealable in both directions. For example, a pad can reveal
its material range; a material can list every pad, audio clip, and analysis
scope that uses it; a promoted pattern can reveal the finding and term that
created it; an explanation can reveal each construction object and comparison.

### 2.2 Three signal layers are context, not kinds of objects

Source, Construction, and Residual are selected signal layers over the same
geometric/object aspect. They belong in the global context strip and the
inspector, never as duplicate project trees. A user can select "the second
chorus hats" and switch which signal is heard or viewed without losing that
selection geometry. `Construction` and `Residual` must name the comparison or
explanation they are relative to.

This prevents the current awkward choice between an analysis-local residual
button and a global source selection. It also makes the reverse/forward loop
legible: edit a construction, then hear the same aspect in the residual.

## 3. Proposed application shell and navigation

### 3.1 The desk

The main window is a project desk with persistent global chrome and ordinary
workspaces beneath it:

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ Project name · save state | undo/redo | tools/snap | transport | tempo   │
│ selected aspect · Source / Construction / Residual | render/task state    │
├──────────────┬──────────────────────────────────────────┬────────────────┤
│ Explorer     │ Workspace tabs / split editor canvas     │ Inspector      │
│ Project      │                                          │ object / route │
│ Library      │ Arrangement | Pattern | Instrument | ... │ evidence / use │
│ Investigate  │                                          │ actions        │
│ Readings     │                                          │                │
├──────────────┴──────────────────────────────────────────┴────────────────┤
│ status: action/shortcut · snap · range · diagnostics · background work    │
└──────────────────────────────────────────────────────────────────────────┘
```

The transport, selected aspect, signal layer, and save/render/task state live
outside workspace tabs. Closing, floating, or splitting an editor cannot hide
the playhead or accidentally create a second truth.

The Explorer and Inspector are dockable but have stable jobs. At narrow
widths they collapse into tabs/drawers; they do not disappear from the
information architecture.

### 3.2 Explorer: one browser with four modes

The present fixed Overview side rail and the Media Pool should converge into a
single **Explorer**, with filters/search rather than a new popout for every
object class.

- **Project:** Song (tracks), Instruments, Patterns, Automation, Buses,
  Markers. This is the forward-production tree.
- **Library:** Materials, Samples, Presets, plugin instruments, favorites,
  tags, locations, missing/relinkable material. This is where a user begins
  when they have a folder, a crate, or an imported recording.
- **Investigate:** Findings, Explanations, Comparisons, coverage hotspots,
  pending/running analyses. This is where the reverse loop begins.
- **Readings:** local and imported readings, verification state, diffs, and
  source-match diagnostics.

Each row has a human name, type icon, state badge, optional color, and a
compact secondary fact. Examples: `Break 03 · 1.74 s · sliced from Source`,
`Pads · 8 pads · route: Drums`, `Beat A · 1 bar · expression`, or `Rhythm
alternative 2 · 124.7 BPM · unaccepted`.

Search is global and typed: `kick` finds names/tags; `type:pattern`; `used-by:
Pads`; `residual`; `source:Like a Pen`; `reading:Jane`. Results are deep links,
not duplicated snapshots.

### 3.3 Inspector: one object, several truthful sections

Selection sets the Inspector target. Multi-selection gives common actions and
a concise count; it does not invent a fake composite object. A normal object
inspector has ordered sections:

1. **Identity** — name, color, type, parent/breadcrumb, availability.
2. **Make / edit** — type-specific controls (pad zone range, clip fades,
   pattern origin, track color, bus controls).
3. **Sound and routing** — audition, mute/solo where meaningful, output route,
   dependencies, render status.
4. **Uses** — reverse links with `Reveal` actions.
5. **Origin and evidence** — source range, command/term/provenance, confidence
   or diagnostics only where relevant.
6. **History** — revision/author/readings; this is informative, not the
   primary name of an object.

The inspector is where provenance earns its keep: quiet for an authored
pattern, immediately available for a deprojected pad, never a modal detour.

### 3.4 Tabs are working surfaces, not object storage

Tabs should use names that answer "what can I do here?":

| New surface | Responsibility | Multiplicity |
| --- | --- | --- |
| **Arrange** | tracks, clips, pattern occurrences, markers, global timeline selection. | One default; additional arrangement views may differ only in viewport/filter. |
| **Pattern** | notes, steps, pattern expression, cycle preview, pattern-local inspector. | Multiple by pattern target; one entity per target/view ID. |
| **Instrument** | pads/zones, material mapping, envelope/loop, routing, performance keyboard. | Multiple by instrument target; retargetable to a pad. |
| **Mixer** | buses, sends, inserts, metering, routing. | Usually one global view; focused-bus views may float. |
| **Automation** | a parameter's curve in musical time. | Multiple by parameter target. |
| **Signal** | waveform/spectrum and direct selection of time/frequency/channel geometry. | Multiple, independently navigable and linkable. |
| **Findings** | analysis alternatives and evidence; preview/apply rather than edit music directly. | Multiple by analysis job/aspect. |
| **Compare** | persistent A/B/R, coverage and excess for one Comparison. | Multiple by comparison. |
| **Reading** | claim graph, verification/diff/import controls. | Multiple by reading. |

`Waterfall`, `Rhythm`, `Components`, `Separation`, and `Loom` become
specialized modes or cards under **Signal**, **Findings**, and **Compare**;
they are not the top-level IA. Their useful names may still appear as tool or
analysis labels. `Media pool` becomes **Library › Materials**. `Piano /
drums` becomes a targeted **Pattern** editor. `Arrangement editor` becomes
**Arrange**. This keeps expert terminology without asking a newcomer to infer
the project model from algorithm names.

## 4. Interaction model

### 4.1 Selection is a noun phrase, actions are verbs

The application maintains a single semantic selection with explicit geometry,
objects, and signal layer. A focused pane may additionally maintain an edit
cursor, hover target, viewport, tool, and drag preview. These must never be
silently substituted for selection.

The selection summary in global chrome reads naturally:

```text
Source · 01:12.400–01:13.960 · 120–5,000 Hz · 2 objects
Construction: "Rhythm A" · 1 bar pattern occurrence
Residual of "Loom pass 2" · 4 selected clips
```

Context actions derive from selected object/geometry and appear in a toolbar,
right-click menu, command palette, and inspector. The action is identical
wherever invoked.

| Context | Primary verbs |
| --- | --- |
| Time/frequency source aspect | audition, loop, **Make Sample…**, **Slice to Kit…**, analyze, explain, compare, add marker. |
| Material or Sample | audition, reveal uses, make sample, slice to kit, add to instrument, place in arrangement, rename/tag/relink. |
| Instrument / Pad / Zone | play, map material, trim, duplicate, route, reveal source/evidence, open instrument. |
| Pattern | edit, play cycle, duplicate, rename, place in arrangement, reveal placements/origin. |
| Clip / Track | arrange, open source/pattern, make unique, route, automate, compare if explanation-backed. |
| Finding / hypothesis alternative | hear evidence, preview construction, compare, **Apply as editable construction…**, retain as reading, reject/dismiss. |
| Explanation / Comparison | hear Source/Construction/Residual, inspect coverage, refresh, export, reveal construction/evidence. |

The word **Extract** should not be a primary verb. It tells the system what to
do internally, not the musician what they will get. Use outcome names and
show the destination before commitment.

### 4.2 The sample creation sheet

Selecting a source range and choosing `Make Sample…` opens a non-modal,
inspectable sheet anchored to the selection:

```text
Make from 01:12.400–01:13.960 of “Source”

  [ One sample ] [ Slice to kit ] [ Make beat ]
  Slice: equal 8 | onsets (preview) | manual
  Destination: New Instrument “Source slice” | Existing Instrument: Pads
  Route: Drums                         After: Open Instrument / Pattern / Stay
  Provenance: exact source range retained
                                           [Cancel] [Create]
```

Defaults are fast but visible: one selection made from the Overview opens
with `One sample`; the last-used destination/route is remembered per project;
no route is silently guessed if multiple appropriate destinations exist.
`Make beat` explicitly says it will create an Instrument, Pattern, and
Arrangement occurrence. Its result may use an adopted tempo hypothesis, which
is shown as a proposal and remains editable.

### 4.3 Reveal Object is a hard product invariant

Every command that creates, imports, promotes, duplicates, or resolves a
durable object returns a product-level `ObjectRef` and a recommended next
focus. The application owns:

```text
Reveal(ObjectRef, RevealIntent)
  ActivateExisting | OpenNew | RetargetCurrent | ShowInspector | SelectOnly
```

The router resolves a typed object reference against the current project,
chooses or creates a descriptor, activates its tab/window, applies the
target, selects the exact sub-object, and scrolls it into view. It may emit a
small completion banner, but the reveal is the confirmation.

Required mappings include:

| Result | Required reveal |
| --- | --- |
| one-shot/kit/pad creation | Instrument target, selected new pad, source range visible in Inspector. |
| chop | Instrument target, new pads highlighted, selected chop preview retained if applicable. |
| make beat | Pattern editor or Arrange according to user choice, with new occurrence selected; Instrument remains one click away in breadcrumb. |
| asset drag to pad | Instrument target and destination pad selected. |
| pattern creation/duplicate | Pattern target selected; first occurrence reveal if created too. |
| rhythm/reconstruction promotion | Findings stays available, newly created construction selected, inspector shows "Derived from…" and diagnostics. |
| comparison creation | Compare target opens on Source/Construction/Residual strip. |
| reading import | Reading target opens on verification summary and imported alternatives; no auto-acceptance. |

`ConstructivePublication`, `SamplePublishedResult`, reconstruction receipts,
comparison IDs, and reading IDs are the existing technical seeds for this
contract. The router must be a small app/service boundary, never a growing
collection of view-specific `open_*` methods.

### 4.4 Audition is consistent and safe

Any object with sound has a visible audition affordance. It uses the existing
audition bus and names the subject: `Audition sample`, `Play pad`, `Hear
construction`, `Hear residual`, `Preview alternative`. Audition is not a
project edit, does not move the canonical playhead unless labelled `Locate`,
and shows its subject in global chrome.

For a context with no well-defined PCM (coverage excess, unresolved effect,
or a query fact), the affordance says `Inspect` and explains why it cannot be
heard. Never leave a play button that does nothing or reinterpret a selection
as source audio without saying so.

### 4.5 Naming, searching, and undo

- Every newly authored object receives a human default name derived from its
  role and source (`Source slice`, `Pads`, `Beat 01`), editable in the
  Inspector and inline where appropriate. Anonymous analytic families remain
  explicitly anonymous until the user names the constructed instrument/pad.
- Renaming changes an authored display name, not evidence identity or source
  claim. Search indexes aliases/tags/display names while preserving stable
  typed IDs underneath.
- A compound action such as `Make beat` is one labelled history entry. The
  completion/reveal does not create history. Undo selects and reveals the
  restored predecessor if it still exists; redo reveals the newly recreated
  result. Async previews create no history until Apply.
- Missing material, stale comparison, incompatible reading, unresolved
  promotion, and render diagnostics are visible states in the object's card;
  they are not silent disappearance or generic failure toasts.

## 5. The reverse-production loop

### 5.1 Findings are proposals, not a second music library

An analysis result belongs to the selected aspect and source revision. It can
be kept as a Finding, compared, or applied. It is not placed alongside drums
and bass as though it were a finished instrument.

A Finding card has:

- scope and source revision;
- what was measured, what the procedure does **not** claim, and diagnostics;
- alternatives ranked for navigation only;
- evidence links and exact audition/preview action;
- `Compare`, `Apply as editable construction…`, `Save to reading`, and
  `Dismiss` actions.

`Apply` opens a small choice sheet when alternatives, timing grids, pitch
choices, routes, or lossy fallbacks are involved. The sheet names the intended
constructed objects and maintains competing alternatives. It never uses the
word "accept" to smuggle a hypothesis into source identity.

### 5.2 Comparison is the bridge, not an optional scientific afterthought

The comparison strip is a first-class working surface:

```text
Original | Construction | Residual     [A/B] [loop aspect] [export pin]
Explained energy 68%  ·  excess 4%  ·  stale after arrangement edit
```

It is the place where an applied reconstruction becomes musically useful:
edit a pad, pattern, automation, clip, or mix control; refresh the
construction; hear the residual change. The detail view can expose coverage
hotspots as navigable aspects, not as a claim of correctness.

### 5.3 Readings are documents with playable references

Readings require their own Explorer mode and pane because their objects have
foreign namespaces, verification states, and alternative/merge semantics.
They should not be hidden under an Export menu. A Reading pane presents:

- source fingerprint and verification tier;
- a claim graph/outline and retained terms;
- playable derived attachment or local re-render where available;
- comparisons and residual evidence;
- diff and import-as-alternatives actions;
- authorship and local annotations.

Importing a reading opens this pane first. It does not silently mutate the
song or promote imported hypotheses.

## 6. Pane lifecycle, targeting, and persistence

### 6.1 Durable descriptor, ephemeral entity

`WorkspaceViewDescriptor` is the durable description of a view: stable view
ID, kind, product target, local presentation state, and link membership. The
runtime entity is an implementation detail. A descriptor's target must be
honoured on creation, replacement, restore, and reveal; fallback to "first
pattern", "first lane", or "first kit" is acceptable only for a deliberate
untargeted *new* command and must be surfaced as such.

View-local state includes viewport, follow, zoom, local tool, unfolded panels,
and non-semantic draft UI. It must not include editable copies of project
objects, transport, global selection, or a hidden second link registry.

### 6.2 Multiplicity policy

| Object/surface | Policy | Close behavior |
| --- | --- | --- |
| Arrange, Mixer | singleton by project by default; floating mirror is the same descriptor/entity. | hide, never destroy musical state. |
| Instrument, Pattern, Automation, Finding, Compare, Reading | multiple by target; opening an existing target activates it unless user asks `Open New View`. | hide for editors; remove transient Finding views only after explicit dismissal. |
| Signal | multiple viewports allowed; optional navigation links. | remove descriptor when closed unless saved as a named lens. |
| Library, Inspector | singleton presentation services, dockable. | hide. |

`Reveal` honours this policy. It should activate a matching target by default,
never create duplicate invisible identities. `Open New View` is an explicit
power-user action for A/B work.

### 6.3 Link policy

Transport is global, period. Selection geometry/signal may be shared through
link groups; viewport, frequency range, and follow are independently opted
into. A pane can always update the global semantic selection even while
unlinked; link facets decide peer broadcast, not whether a normal click is a
selection. A delivered selection must not echo around a link group.

Current pane-session work moves toward this model. The product requirement is
that a link badge tells the user exactly which facets are shared and that
changing source/construction/residual does not secretly alter geometry.

## 7. Canonical journeys

### 7.1 “This loop has a great snare; make a playable beat”

1. Open material in **Signal** or use the Source track in **Arrange**. Drag a
   time range; the global context strip displays its exact duration.
2. Click `Make Sample…`, choose `Slice to kit`, preview onset cuts, name the
   Instrument `Loop drums`, choose `Drums` route, then Create.
3. The app reveals **Instrument › Loop drums**, highlights new pads, and the
   Inspector says each pad is sliced from exact source frames. Pads audition
   immediately.
4. Choose `Create beat from selected pads` or open a new **Pattern**. The
   pattern is placed in **Arrange** only if the user chose that outcome.
5. The user edits while looping. The same bounce-on-play render is heard in
   playback and export. Undo returns to the pre-kit project and closes no
   unrelated panes.

### 7.2 “Turn a rhythm reading into an editable alternative”

1. In **Findings**, select an anonymous rhythm pattern over a source aspect.
2. Hear evidence and preview alternatives. A grid/phase choice sheet makes
   its tempo and phase proposal visible.
3. `Apply as editable construction…` creates a neutrally named Instrument,
   Pattern, routed track, and optional arrangement occurrence. It reveals the
   requested Pattern or Arrange target, while a breadcrumb points back to the
   Finding.
4. The user renames it `Hats` if they choose. The evidence remains "anonymous
   family"; naming is authorship, not a retroactive inference.
5. Open **Compare** to hear source/construction/residual and retain the
   resulting comparison in a Reading if useful.

### 7.3 “Find why my reconstruction fails”

1. Open a persistent **Compare** object for an explanation.
2. Click a coverage hotspot. Signal and Arrange navigate to the same aspect;
   they retain their independent zoom choices.
3. Inspector lists contributing clips, patterns, and evidence. The user opens
   the appropriate target, edits it, and refreshes comparison intentionally.
4. Staleness is visible until refresh; the old result is still inspectable.

### 7.4 “Receive a reading”

1. Import opens **Reading** with a verification state, not a silent merge.
2. With matching material, the user can hear/re-render constructions and
   inspect residual/comparisons. With a mismatch, the surface reports the
   exact refusal.
3. `Bring alternatives into project…` previews typed additions. Imported
   hypotheses coexist with local ones and remain qualified by reading.

## 8. Migration and convergence plan

This is a large change in product organization, not a rewrite. Keep the
existing good engines and execute it in slices that leave a usable desk after
each convergence wave.

### Wave A — make outcomes visible

1. Introduce a GPUI-neutral `ObjectRef`/`RevealIntent` and application
   `ObjectNavigator`. Map all existing constructive/reconstruction/comparison
   receipts to it.
2. Install `SampleFocusCallback` in Browser and Instrument factories. Route
   `SampleResultFocus` into the navigator. Overview source actions call the
   same router after successful publication.
3. Change current labels: `One-shot` → `Make sample`; `Chop ×8` → `Slice to
   kit`; `Make 1-bar beat` → `Make beat`. Show destination and next focus.
4. Ensure every reveal also updates inspector/selection and has a test that
   asserts the correct target descriptor is active or created.

### Wave B — establish the information architecture

1. Add Explorer modes and a universal Inspector shell over existing Asset,
   Sampler, Pattern, Arrangement, Mixer, and analysis models.
2. Move the fixed Overview rail's material/actions into Explorer/context
   actions. Keep the source timeline as a Signal/Arrange surface, not an app
   sidebar.
3. Rename workspace surface titles and map descriptor kinds to product
   responsibilities. Preserve old persisted kind IDs with migrations.
4. Make target-specific constructors obey descriptor target/state. Do not
   claim restore support until state is applied and written back.

### Wave C — unify session delivery and local state

1. Finish routing `PaneSessionBinding` deliveries through every pane host;
   eliminate duplicate link routers and legacy `open_*` popout paths.
2. Make global transport/audio/status and semantic selection the only shared
   presentation authorities. Keep local viewport/tool/gesture state local.
3. Use project publication fanout for all live editors; no pane keeps a
   mutable project copy. Existing browser/sampler replacement logic is an
   acceptable bridge but should converge on explicit pane adapters.

### Wave D — make reverse work persistent and navigable

1. Surface Findings, Explanation, Compare, and Reading panes using their
   existing pure models and project codecs.
2. Promote via choice sheets and reveal receipts; provide permanent evidence
   breadcrumbs from constructed objects.
3. Add coverage hotspot navigation and A/B/R strip to the Inspector/Compare
   surface.

### Wave E — retire compatibility shells only after parity

1. Remove direct `Workbench::open_visualizer`, `open_arrangement_editor`,
   `open_sequencer_editor`, `open_mixer`, `open_automation`, and
   `open_assets` as user-facing navigation routes. They become navigator
   adapters or disappear.
2. Retire fixed six-view assumptions and the duplicate legacy pane caches.
3. Retire source-only layer rail labels once Explorer/Signal replaces them.

## 9. Explicit retirements and refusals

Retire these product ideas, even if compatibility code remains briefly:

- **The Overview as the place where every action lives.** It can be a useful
  source/arrange view; it is not the application's IA.
- **Algorithm names as primary navigation.** HPSS, Loom, components, and
  rhythm remain truthful tools under Signal/Findings/Compare, not the user's
  only map of their project.
- **Success-by-toast or success-by-revision.** A durable result that is not
  revealable is product-incomplete.
- **A source range as implicit global state.** A selection is an explicit
  aspect with signal layer and must be inspectable/shareable.
- **First-object fallbacks as restored targets.** A reopened automation view
  must not silently edit the first lane; a sampler must not silently choose
  the first kit when its descriptor names another.
- **Instrument names inferred from analysis.** Keep anonymous evidence
  anonymous; user naming is separate authorship.
- **A second realtime graph or a special playback truth for views.** Playback
  remains the bounce/render path; panes ask for global transport/audition.
- **A general-purpose in-process scripting language that mutates a project.**
  Commands remain the edit language; pattern/curve/aspect/query terms remain
  data and pure evaluation.

## 10. Acceptance criteria

The redesign is not complete because the vocabulary appears in a mockup. It
is complete only when a musician can demonstrate the following without
knowing implementation names:

- Select a source range, make a sample, and land on the exact selected pad
  with its source range and route visible.
- Slice a loop, make a beat, hear it on the next playback, open its pattern,
  and reveal its arrangement occurrence and instrument in both directions.
- Search a material and find all clips/pads that use it; relink missing media
  without losing its identity.
- Select a pattern occurrence, edit the definition or make it unique, and
  understand which action changed which object.
- Promote a rhythm/reconstruction alternative without it being falsely named
  or silently accepted; hear and edit its construction, then hear residual.
- Create, reopen, and refresh a persistent comparison; navigate its hotspots
  to exactly the relevant musical/editor targets.
- Import a reading and inspect alternatives without changing the project
  until an explicit, undoable import/apply command.
- Close, float, restore, and duplicate panes without duplicating project,
  transport, selection, or object identity.

If any one of these ends at an unlabeled status string, an unaddressable
numeric identifier, a new unexplained popout, or a silent fallback target,
the desk has not yet met this architecture.
