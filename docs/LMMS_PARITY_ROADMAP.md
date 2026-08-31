# 85% of LMMS, plus the reasons audec should exist

Status: repository audit on 2026-08-31. This is a delivery roadmap, not a
claim that audec is already a general-purpose DAW.

LMMS is a useful reference for the everyday, pattern-oriented production
surface: arrange clips, make beats and notes, automate parameters, route and
mix sound, use instruments/effects, render a file, and reopen the project.
audec should meet that surface where it serves its users, but should not clone
LMMS's architecture, widgets, formats, or product identity. Its differentiator
is the reverse direction:

```text
recording → evidence → alternatives → editable construction → render → residual
                         ↑                                  │
                         └────── listen, challenge, revise ─┘
```

The useful meaning of **"85%"** is therefore not 85% of LMMS source files or
menu entries. It means an electronic-music maker can complete at least ten of
the twelve ordinary workflows below entirely in audec, without a hidden demo
model, hand-editing a project file, or needing a second DAW for the middle of
the task. The remaining 15% deliberately excludes broad compatibility and
ecosystem work that would displace the reverse-DAW mission.

## Status vocabulary

| Status | Meaning |
| --- | --- |
| **Implemented** | In the shipping binary, connected to loaded project material or an executable render path, and usable as a normal workflow. |
| **Core only** | Validated/tested domain and/or render code exists, but the application does not yet make it the authoritative live project state. |
| **UI only / demo-backed** | A real editor exists, but its current launch path owns a local or demonstration graph rather than the opened project's truth. It must not be counted toward 85%. |
| **Prototype** | Useful exploratory vertical slice, deliberately restricted or lacking a production contract. |
| **Missing** | No sufficiently coherent implementation boundary exists yet. |

"Core only" is valuable: audec already has unusually good typed, deterministic
foundations. It is nevertheless not user-visible parity until one
`DawProject`/`ProjectSession` owns the live state, the renderer consumes its
compiled schedule, and the UI edits that same object.

## What audec already has

The repository is not starting from a blank DAW canvas. It has:

- retained decoded PCM, exact sample coordinates, waveform pyramids, and
  pixel-aware spectral-tile planning;
- sample-accurate transport and loops, a real audio audition/playback host,
  and deterministic WAV export primitives;
- an editable arrangement model with non-destructive audio/pattern/automation
  regions, exact trim/slip/split/duplicate/fades, stretch metadata, undo, and
  typed persistent identities;
- a PPQ sequencer with tempo and meter maps, notes, steps, sample triggers,
  probability, ratchets, swing, deterministic humanization, and block-exact
  scheduling;
- compiled automation, a routing/mixer graph, plugin indexing/runtime
  contracts, asset provenance/relinking, aggregate project validation, and an
  immutable render schedule/reference renderer;
- an AIR ontology for source spans, pitch trajectories, transforms,
  modulation, relations, evidence, provenance, uncertainty, and competing
  hypotheses;
- live GPUI views for the forensic workbench, CQT/FFT waterfall, rhythm and
  recurrence, NMF components, HPSS, Loom reconstruction, arrangement editing,
  piano/step editing, mixer controls, automation curves, and tabbed/tear-off
  analysis views.

The important qualification is ownership: `Workbench` currently opens a real
source-material arrangement editor, while the sequencer, mixer, and automation
popouts are seeded local demo graphs. `DawProject` compiles a coherent
cross-domain snapshot, but is not yet the project entity rendered by the audio
host or edited by the visible DAW surfaces.

## Capability matrix

### Everyday DAW surface

| Capability | Current audec evidence | Status | What turns it into parity |
| --- | --- | --- | --- |
| Open/import a recording | FLAC decoding and native picker; retained PCM feeds analysis and original-material playback. | **Implemented (FLAC)** | Add WAV/AIFF/MP3/Vorbis import through one decode/seek path, import-as-asset, and explicit resample policy. |
| Project save/open | Versioned material/workspace/artifact manifest and separate `DawProject` save intent exist. | **Core only** | One `.audec` document containing arrangement, patterns, automation, mixer, AIR, asset references, UI state and migrations; Save/Save As/recent/recover in app. |
| Asset browser and relinking | Content fingerprints, metadata, usage, duplicate discovery, search, missing-media and deterministic relink core. | **Core only** | Dockable browser, drag/drop import, waveform preview, tags/collections, missing-media repair UI. |
| Global transport | Exact frame transport, play/pause/seek/loop and shared source audition are active in the workbench/lenses. | **Implemented for source material** | Make it the `DawProject` transport: tempo display, metronome, count-in, punch, record-arm, preroll, and editor-safe focus rules. |
| Multitrack arrangement | Persistent tracks/clips and non-destructive transactions; arrangement GPUI editor has selection, mouse/keyboard edit operations, zoom/pan/fit/snap and undo. Loaded source becomes a real audio clip. | **Implemented for source clip editing; core-only for project** | Project-owned multi-track editor with track creation/reorder/color/height, marquee, tool modes, drag move/trim/slip/fade/crossfade, markers, loop/punch, minimap, and rendered clip audio. |
| Audio clips | Source ranges, gain, reverse, channel mapping, loop modes, fades, rational stretch metadata; reference renderer handles resample/reverse/fades/maps. | **Core only** | Asset-backed clip renderer in live graph; waveform proxies, drag/drop, clip gain/fades/crossfades, channel/warp UI, offline high-quality stretch/pitch algorithms. |
| Pattern clips/library | Arrangement pattern regions and sequencer `PatternLibrary`/instances exist with linking validation. | **Core only** | Pattern browser, create/duplicate/make-unique, instance overrides, double-click descent, clip looping and arrangement placement UI. |
| Piano roll | Notes, velocity, articulation and per-note expression core; GPUI piano roll supports add/select/move/resize/delete, quantize, grid, zoom/pan, undo. | **UI only / demo-backed** | Bind to selected project pattern; note audition; velocity/expression lanes, scale/chord tools, lasso, copy/paste, record-to-pattern, inspector. |
| Beat/step/drum sequencer | Step lanes/events include triggers, probability, ratchets, microtiming/gate/pitch/gain/pan/swing; GPUI step editor works over its local sequencer. | **UI only / demo-backed** | Project pattern binding, pad/sample mapping and audition, lane management, pattern clips, recording, per-step editing/automation, browser drag-to-pad. |
| Tempo and meter | Exact rational beat↔frame conversion, tempo/meter changes and musical display mapping. | **Core only** | Global tempo/meter editor and ruler, tempo automation policy, detected tempo alternatives/adoption, metronome and count-in. |
| MIDI input/output | Notes can be scheduled as internal events; no device layer. | **Missing** | MIDI device discovery, timestamped input, record/thru, MIDI output, mapping, clock/transport policy. |
| Audio recording | Transport has a recording mode only. | **Missing** | Input device selection, armed tracks, disk writer, takes/comping, latency calibration, punch/loop record, recoverable interrupted takes. |
| Sampler/instruments | Trigger targets and sample assets exist; render diagnostics explicitly report that sequencer events need instruments. | **Core only** | First owned sampler (one-shot/loop/envelope/filter/pitch/voice stealing), then a modest synth/effect palette; live/offline graph nodes. |
| Mixer and routing | Buses, sends, inserts, gain/pan/mute/solo, cycle checks, latency plan and command history exist. Mixer GPUI surface edits a local backend. | **UI only / demo-backed** | Project-bound channel strips, real post-DSP meters, routing/send editor, groups/returns, record/monitor state, graph recompilation indication. |
| Effects and instruments via plugins | Safe CLAP discovery, identity/state/ports/parameter/runtime-worker contracts, quarantine and missing-plugin preservation. No dynamic library load. | **Core only** | Start with one owned test CLAP effect, execute off control/audio threads under the contract, then instruments/sidechains/editor hosting. |
| Automation/modulation | Typed addresses, sample/beat-domain curves, interpolation, smoothing, compile-without-allocation and write-mode core; GPUI curve editor edits a local graph. | **UI only / demo-backed** | Unified project parameter registry, bind from every control, project lanes/clip lanes, record/touch/latch/write, render schedule events, live DSP application. |
| Undo/redo | Independent arrangement, sequencer, mixer and automation histories plus aggregate transaction validation. | **Core only / partly implemented** | A single user-visible cross-domain project history; one drag = one command; undo covers UI selection-independent edits and survives save/reopen. |
| Realtime audio engine | Source PCM playback and independent audition bus are live. Render schedule and reference PCM renderer exist. | **Prototype** | Publish immutable compiled graph to callback, use assets/instruments/plugins/automation/mixer, lock/allocation-free callback, meters/xrun diagnostics, device switching. |
| Offline bounce/export | Deterministic WAV encoding/export and render scheduling/reference render exist. | **Core only** | Export dialog, selection/track/stem/master modes, tail/dither/sample-rate options, cancellation/progress, diagnostics/reproducibility manifest. |
| Keyboard/menu/command palette | Focused editor keymaps exist; standard actions are incomplete and no shared command registry/menu/command palette exists. | **Prototype** | Discoverable action registry, menus/context menus, shortcut editor, correct focus precedence, accessibility and complete keyboard coverage. |
| Flexible docking/windows | Guise tabs/splits/tear-off/dock-back model exists for six analysis views; visible app workspace opens those items as one tabbed group. | **Implemented for analysis lenses** | Extend descriptor registry to arrangement, browser, piano, steps, mixer, automation, inspector, and duplicated targeted lenses; save/restored layouts and link groups. |

### Reverse-DAW surface: audec's advantage

| Capability | Current audec evidence | Status | Product requirement |
| --- | --- | --- | --- |
| Evidence atlas | Waveform, loudness, centroid, transient, stereo width/correlation and shared playhead on retained PCM. | **Implemented** | Integrate annotations/markers and source/claim selection into the same project selection. |
| Resolution-honest spectral inspection | Waterfall has retained numeric dB field/settings; visible-range spectral tiles are keyed by source, recipe, range, and pixel LOD. | **Implemented / actively maturing** | All spectrum-facing views use cancellable visible-range tiles; loading/refining is explicit and zoom never enlarges stale low-res imagery. |
| Rhythm deprojection | Multiband novelty, tempogram, tempo/phase/downbeat alternatives, hit families, medoid examples and patterns; GPUI rhythm lens with async retained PCM. | **Implemented as provisional inference** | Editable beat grid + family lanes that instantiate project step patterns without erasing source timing or alternatives. |
| Recurrence/components | Onset-centered recurrence clusters, NMF and NMFD hypotheses, source spans and fit/limitations are present. | **Prototype** | Selection-local, editable component masks/templates; comparison and residual for every accepted construction. |
| HPSS and residual | Selection-local phase-preserving STFT/HPSS, original/harmonic/transient/null audition. | **Prototype** | Tiled whole-material complex cache, editable masks, source/lens links and production graph export. |
| Loom reconstruction | Editable recurring-event templates (mute/shift/gain), overlap-add reconstruction and audible residual. | **Implemented vertical slice** | Multi-channel/separated or generated template sources, source-event relationship editor, route Loom voices through project arrangement/mixer. |
| Pitch/modulation/effect evidence | CQT, YIN/spectral pitch candidates/tracks/modulation and typed AIR transforms. | **Core only** | Pitch/gesture lenses, editable conversions to authored notes/automation with uncertainty and original-contour visibility. |
| ML separation/transcription | Pinned model manifests, worker protocol, staging, cancellation, provenance and audited candidate strategy. No model is downloaded or executed. | **Core only** | Opt-in worker adapters plus goldens: Basic Pitch, Beat This!, then HTDemucs/MLX and specialized drums; outputs imported as ranked AIR claims, never presumed stems. |
| AIR and uncertainty | Typed objects, transforms, evidence, competing hypotheses, provenance and epistemic limitations; aggregate bindings validate links. | **Core only** | Project persistence, inspector, claim state/actions, alternative compare/merge, confidence calibration and visible authored-vs-inferred grammar. |
| Perceptual / phenomenotechnique tools | Vision/ontology define attributed experience and crossmodal bindings; no interactive lexicon or vEAR lens yet. | **Missing** | Personal descriptors, time-linked observations, calibrated audiovisual mappings, A/B and multi-author comparison—always attributed, never universalized. |

## The twelve 85% workflows and acceptance gates

An individual feature is not "done" because its data structure compiles. Each
workflow below is an acceptance gate: it must be demonstrated against a saved,
reopened project and accompanied by an automated test at the model/render
boundary where practical.

| # | Workflow | Acceptance gate |
| --- | --- | --- |
| 1 | Start a track from material | Import FLAC/WAV/AIFF; it appears once in the asset browser and as an arrangement clip; source playback, selection, loop and visible waveform are sample-consistent. |
| 2 | Build an audio arrangement | Create/reorder tracks; drag/duplicate/split/trim/slip/fade/loop clips; undo/redo returns exact source ranges; live playback and offline bounce agree within declared renderer tolerance. |
| 3 | Make a drum pattern | Drag an owned asset to a pad/lane, program accents/probability/ratchets/microtiming, place repeating pattern clips, and hear it through the project mixer. Same seed produces the same result in realtime and bounce. |
| 4 | Write notes | Create a note pattern in piano roll, set velocity/duration/expression, quantize or humanize it, route it to an owned sampler/instrument, and edit it from either pattern or arrangement without duplicate truth. |
| 5 | Establish musical time | Set tempo/meter, insert a tempo change, use bar/beat snapping and metronome, then render across the change without drift. Detected tempo candidates can be adopted as an explicit authored choice. |
| 6 | Shape motion | Expose a mixer/instrument/clip parameter once, create beat- or sample-domain automation from it, edit/record a curve, and verify identical scheduled parameter offsets in offline render. |
| 7 | Mix a small song | Route at least eight source/pattern tracks through groups/returns; adjust gain/pan/mute/solo/sends; see real meter values; bounce master and selected stems with latency/tail accounting. |
| 8 | Reopen safely | Save, quit, reopen and relink moved material by content identity; preserve clips, patterns, automation, mixer, AIR provenance, windows and missing-plugin placeholders without executing untrusted code. |
| 9 | Use a trusted plugin | Scan a CLAP out of process, instantiate a known test plugin under a bounded processing contract, automate it, save/restore it, and fall back visibly and safely on crash/missing state. |
| 10 | Record and revise *(late 85% gate)* | Record a short external audio take into an armed track with latency compensation, make a non-destructive comp/edit, and bounce it. If recording is deferred, all other 11 workflows must pass and the product must state that scope. |
| 11 | Decompile a beat | On a mixed electronic recording, show ranked tempo/phase alternatives, onset families and source evidence; accept/edit selected events into a project drum pattern while retaining unquantized originals and a reject/revert path. |
| 12 | Test an explanation by ear | Route a Loom/HPSS/NMF/ML-derived hypothesis into the arrangement, edit it, rerender it and audition/measure the residual against the exact source span. The UI labels the result as measurement, inference, or authored construction. |

For the milestone called **85%**, gates 1–9, 11 and 12 are mandatory. Gate 10
may be the one deferred workflow, provided no claim is made that audec is a
recording DAW. MIDI I/O and extensive third-party plugin compatibility are
important expansion work, but are not a substitute for the reverse-DAW gates.

## Ordered milestones

### M0 — Make one project real

**Outcome:** replace the current collection of excellent islands with one live
`ProjectSession`/`DawProject` owner.

- Create the GPUI project/session façade proposed in `DAW_ARCHITECTURE.md`.
  It is the only mutable project state; editor entities retain only target IDs,
  viewports, focus and transient gestures.
- Move source import into the asset registry and bind it to arrangement/AIR
  identities. Migrate `Workbench` playback and selection to shared session
  transport.
- Wire arrangement, sequencer, mixer and automation views to injected project
  backends. Remove their demo launch paths from ordinary application use.
- Publish one immutable render schedule on edits. Keep source PCM playback as
  an explicit fallback only while graph execution is incomplete.
- Implement `.audec` save/open around all domain DTOs and workspace snapshot;
  preserve missing assets/plugins and migrations.

**Exit evidence:** create an audio track, a note/step pattern, a mixer route
and an automation lane; close/reopen; every editor shows the same state and
the project reports one revision/journal. No visible "demo" state remains.

### M1 — A usable electronic arrangement desk

**Outcome:** gates 1, 2, 5 and the editing half of 8 pass.

- Dockable Arrangement, Asset Browser, Inspector and transport shell; target
  descriptors replace the six-singleton workspace enum for project editors.
- Finish production interaction: track management, marquee/time selection,
  mouse move/trim/slip/fade/crossfade, tool modes, clip loop/reverse/gain,
  markers and minimap.
- Build the canonical visible-range waveform/spectral proxy and make local
  panning independent of global playback in all editors.
- Add media decoding/metadata/seeking through a single importer and explicit
  conversion policy. Use retained proxy/cache keys, not painted bitmaps.
- Ensure source-clip live render and reference export match exactly for the
  supported transform subset; diagnose unsupported transforms rather than
  silently approximating them.

**Exit evidence:** produce a 30-second, eight-clip audio arrangement with
edits, fades and a tempo change; live monitor and bounced WAV agree at tested
sample points; save/reopen after moving one source asset and relink it.

### M2 — First constructive voice and patterns

**Outcome:** gates 3, 4 and 5 pass without external plugins.

- Implement a modest owned sampler node first: mono/stereo assets, one-shot
  and loop modes, pitch/gain/pan, envelope/filter, voice allocation and exact
  scheduled events. Do not begin with a giant synth catalogue.
- Bind piano and step editors to selected pattern definitions and placements;
  add pattern browser, make-unique, drag-to-arrangement, copy/paste, lane/pad
  management, note velocity/expression and audition.
- Connect tempo/meter map to global ruler, bar/beat snap, metronome/count-in
  and arrangement pattern compilation.
- Compile pattern clips, notes, steps and sampler voices into the actual
  realtime/offline schedule with a deterministic seed.

**Exit evidence:** make a four-bar drum-and-bass sketch entirely in audec,
duplicate it into a short arrangement, bounce it twice with same seed and
obtain identical PCM for owned nodes.

### M3 — Mix, automation and reliable export

**Outcome:** gates 6, 7 and export/reopen portions of 8 pass.

- Adopt one project-wide `ParameterAddress` registry; all knobs, inspector,
  automation lanes, MIDI mapping later and plugin parameters resolve through
  it.
- Connect mixer GUI to project graph and audio engine: buses, groups, returns,
  sends, inserts, mute/solo, real meters, latency and graph-rebuild state.
- Connect automation editor to real lanes; add clip/track/bus targets,
  value/time snapping, curve tools and touch/latch/write capture.
- Add export UI for master, selection, track and stems with WAV format,
  dither, tail, cancellation/progress and reproducibility diagnostics.

**Exit evidence:** automate a send and a filter-like sampler parameter across
tempo changes, mix eight tracks plus one return, export master/stems, and
verify the rendered schedule includes the expected sample-offset events.

### M4 — Reverse material becomes constructive material

**Outcome:** gates 11 and 12 pass; this is the point where audec overtakes a
normal lightweight DAW instead of merely catching up.

- Make AIR claim/evidence/provenance inspectable in the browser/inspector and
  persistent in the project; distinguish signal, inference and authored
  marks with stable visual grammar.
- Promote rhythm outcomes only through an explicit *deproject to pattern*
  operation: preserve source span, candidate tempo/phase/meter alternatives,
  confidence and original microtiming; generate editable steps/events with
  links back to evidence.
- Route Loom, HPSS and NMF results as auditionable project sources. Every
  accepted/editable construction exposes original/reconstruction/residual and
  a non-destructive reject/revert action.
- Complete tiled complex-STFT/HPSS cache and selection-local masks; make
  pixel-aware recompute cancellation visible rather than blurring old images.
- Add pitch/gesture conversion to authored notes/automation as a reversible,
  uncertainty-bearing operation, not automatic MIDI transcription.

**Exit evidence:** take a mixed four-on-the-floor section, create an editable
kick/snare/hat hypothesis lane and a recurring texture hypothesis, alter their
timing/gain, then A/B original/reconstruction/residual while each generated
object navigates back to exact evidence spans and settings.

### M5 — Opt-in ML and safe external sound

**Outcome:** ML and plugins strengthen explanations without becoming opaque
or destabilizing the core.

- Ship only manifest-driven, user-download model adapters with fixture
  goldens, cancellation, memory limits and model/license/training provenance.
  Recommended order: Basic Pitch and Beat This!, then HTDemucs/MLX, dedicated
  vocal/drum models, sample matching and synth-patch proposals.
- Represent overlapping/generative outputs honestly: model output vocabulary
  is not original-session track identity; retain additivity/null/error metrics
  and bleed probes.
- Implement the trusted CLAP vertical slice described in `PLUGIN_HOST.md`:
  scanner process, owned tiny test plugin, bounded runtime contract, offline
  parity, automation, crash recovery and missing placeholder. Expand formats
  only after this passes.
- Add MIDI I/O and recording only when their timestamps, latency and recovery
  behavior can meet the same project/render invariants.

**Exit evidence:** a user can install one audited model and one known plugin,
run both against a project, cancel either safely, reopen the project on a
machine where they are absent, and still inspect/save all claims/state.

### M6 — Phenomenotechnique and collaborative reading

**Outcome:** the tool can capture what a listener hears without laundering it
into fake objective signal metadata.

- Add attributed perceptual notes linked to time/frequency/objects: e.g.
  "approaching," "mucusy," "cold," or "dance-demanding," including author,
  context and confidence.
- Add optional, user-calibrated vEAR/crossmodal bindings such as onset→impact,
  reverb spread→defocus, filter sweep→chromatic displacement and stereo
  motion→vector motion. These are personal instruments, not universal
  psychoacoustic claims.
- Support lens forks, named link groups, side-by-side hypothesis A/B, and
  exportable evidence packages so collaborators can compare readings.

**Exit evidence:** two listeners can keep distinct readings of the same
passage, audition the exact shared evidence and generated render, and compare
without one interpretation overwriting the other.

## Architecture gates that may not be skipped

These are sequencing constraints, not polish:

1. **One canonical state and history before more editor controls.** Do not
   wire another toolbar directly to `Workbench` or create a new local demo
   backend. The project owner validates a transaction, advances a revision,
   and publishes a snapshot once.
2. **One render compiler before realtime features multiply.** Every audio
   source must lower into the same immutable schedule used by realtime and
   offline modes. Unsupported DSP must emit a visible diagnostic/silence or
   explicit fallback—not a plausible but different result.
3. **One parameter address before automation/MIDI/plugin UI expands.**
   `ParameterAddress` must identify clip, mixer, instrument, plugin,
   decomposition and lens targets unambiguously across persistence and render.
4. **Use sample frames for audio and PPQ for musical intent.** Compile both
   deterministically into half-open project-frame windows; never let editor
   pixel positions or floating seconds become project truth.
5. **Do not conflate facts, hypotheses and authorship.** A hit family is not a
   drum label, a component is not a stem, and a transcription is not a MIDI
   original. The conversion into construction must be explicit and reversible.
6. **No realtime unsafety for convenience.** The callback performs no
   allocation, locks, I/O, graph building, model work, plugin scanning or UI
   work. Progress, errors, cache/refinement and xruns remain visible.
7. **Resolution follows view.** Spectra/waveforms are numeric query results
   keyed by exact source and analysis recipe; zoom asks for new detail. A
   stretched low-resolution image is both a visual and epistemic bug.

## Deliberately out of scope for the 85% milestone

- Binary/project compatibility with LMMS, FL Studio, Ableton, Logic, VST3 or
  Audio Unit sessions; import/export of interchange formats can be evaluated
  later, but audec's AIR/provenance model must not be flattened to imitate one.
- Recreating LMMS's bundled-instrument breadth, skins, beat/bassline-editor
  history, controller ecosystem or plugin database UI. A small owned sampler
  and a reliable effect path are higher leverage.
- Every third-party plugin format and unrestricted in-process plugin loading.
  CLAP-first, isolated/contracted execution is the intended path.
- Distributed/cloud source separation or uploading private recordings/sample
  libraries by default. Models are opt-in local workers with explicit assets
  and provenance.
- A claim to recover the original DAW session, exact source stems, producer
  intent, sound-design patch, or perceptual truth from a mastered mix.
- Automatic genre/instrument labels as authoritative output. They may be weak
  suggestions with evidence and uncertainty.
- Notation engraving, score publishing, video scoring, surround/immersive
  production, network collaboration/CRDT editing, mobile/tablet versions,
  exhaustive accessibility certification, or a marketplace before the core
  project/render loop is trustworthy.

## Definition of done for the milestone

audec can reasonably say **"a reverse DAW with the everyday production surface
of a lightweight pattern DAW"** when the mandatory acceptance gates pass on a
fresh machine and the following statement is true:

> A person can import or derive material, build and edit a small electronic
> arrangement with audio, steps and notes, automate and mix it, save/reopen
> it, render it deterministically, and then use evidence-linked deprojection
> and residual listening to challenge or rebuild part of a recorded mix.

Until then, the truthful description remains: **a promising native reverse-DAW
workbench with substantial tested production foundations and several active
editor prototypes.**
