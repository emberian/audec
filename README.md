# audec

**A reverse DAW for taking recorded sound apart without pretending the evidence is more certain than it is.**

![audec arrangement workbench](docs/screenshots/audec-arrangement.png)

audec is a native, multiwindow audio workbench built with Rust and GPUI. Give it a finished recording and it turns the mix into inspectable measurements, recurring-pattern hypotheses, editable event templates, audible reconstructions, and residuals. The goal is not to claim access to the lost original session. It is to build the most useful explanation of the sound that the evidence supports—and let you listen to where that explanation fails.

> **Project status:** audec is an early macOS alpha under active development. It now has real arrangement, sequencing, automation, mixer, project, instrument, and render foundations, but several editors are still being connected to one audible aggregate project. It is not yet a production replacement for an established DAW or source-separation product.

The project is developed at [ember.software](https://ember.software) and dual-licensed under Apache-2.0 or MIT.

## Why a reverse DAW?

A DAW begins with tracks, notes, automation, effects, and routing, then renders them into audio. audec starts with the audio and works in the other direction:

```text
recording → measurements → events and gestures → patterns and sources
          → editable hypotheses → reconstruction + unexplained residual
```

This is closer to decompilation than transcription. A software decompiler does not recover privileged original source code; it produces a useful intermediate representation with address mappings and uncertainty. In the same way, audec is growing an **Auditory Intermediate Representation (AIR)** whose objects remain linked to the exact samples and transforms that proposed them.

The working loop is:

```text
listen → isolate an aspect → form a claim → test it by ear
       → deproject it into editable structure → rerender → inspect the residual
```

Signal, inference, and experience stay distinct. A spectral peak is not automatically a pitch. Similar attacks are not automatically the same instrument. A low-rank component is not automatically a stem. Descriptions such as “cold,” “approaching,” or “dance-demanding” belong in the system too, but as attributed perceptual readings rather than universal measurements.

## What works today

The current GPUI application provides a synchronized whole-material workbench and five analysis lenses in a Guise tab/split workspace. Lenses preserve their entity state when rearranged and can also open in independent native windows:

- **Workbench atlas** — stereo waveform, log-frequency energy, loudness, spectral centroid, transient, stereo-width and correlation lanes, plus measurements at the shared playhead.
- **Waterfall** — a retained numeric dB field with independent time and log-frequency viewports, adjustable FFT size, window function, dB ceiling, and display range. Analysis settings recompute the field instead of merely stretching an image.
- **Rhythm deprojection** — asynchronous multiband novelty analysis, ranked tempo alternatives, beat/downbeat and pattern-start evidence, anonymous hit families, exact source spans, and auditionable family medoids. Families remain unnamed until stronger evidence exists.
- **Components** — deterministic NMF spectral templates and activation histories presented as recurring mixed-audio hypotheses, with fit information rather than instrument names.
- **Decomposition** — reconstructible, selection-local HPSS with audible original, tonally sustained estimate, transient estimate, and mixture null. Analysis is currently capped at 30 seconds while tiled complex-STFT caching is unfinished.
- **Loom** — aligned, phase-preserving excerpts from recurring attacks deprojected into an editable event sequence. Clusters and events can be muted, shifted, or gain-adjusted; audec immediately overlap-adds a new render and exposes the residual.

All lenses share playback and seeking while retaining their own view range. They can be paused on a detail, panned independently of transport, zoomed, and asked to follow the playhead again. The workbench timeline has the same independent viewport, sample-accurate selection and looping, and visible-range waveform/feature queries. Its log-frequency lane is regenerated from the exact visible samples at the current pixel width, so zooming reveals new spectral detail instead of stretching a whole-song bitmap. The waveform is backed by a multiresolution min/max/RMS pyramid over retained PCM for the same reason.

Loom is deliberately described as **event-template resynthesis**, not recovered stems. Its templates are excerpts from the mixture and may contain overlapping voices, room sound, and effects. NMF likewise finds recurring structure in a nonnegative time-frequency field; it does not establish source identity.

The application also exposes functional native DAW editors:

- **Arrangement** — exact-frame audio, pattern, and automation clips with selection, snapping, pan/zoom, move, trim, split, duplicate, delete, undo/redo, and a provenance inspector. The loaded recording appears as a real source clip.
- **Piano roll and drum sequencer** — editable note and step-pattern views backed by the PPQ sequencer, including quantize, swing, tempo/meter, microtiming-aware scheduling, and history.
- **Mixer** — command-backed gain, pan, mute, solo, inserts, sends, routing, and undo/redo. Disconnected meters and plugin slots are labeled honestly.
- **Automation** — typed parameter lanes with points, curve shapes, binding modes, compiled-value preview, and reversible edits.

The repository contains the UI-independent foundations that these views are being joined onto:

- a sample-accurate project session with typed track, lane, clip, cluster, and event IDs; transport, loop, selection, snapping, revisions, and reversible commands;
- a persistent arrangement model with typed audio, pattern, and automation clips; trim, slip, split, duplicate, fades, stretch metadata, overlap policies, atomic edits, and undo/redo;
- a PPQ sequencer for piano-roll notes, step/drum/sample lanes, tempo and meter changes, quantize, swing, deterministic humanization, ratchets, and exact loop-boundary scheduling;
- realtime-safe compiled automation curves, a validated mixer/routing graph, deterministic offline WAV rendering, and safe plugin scan/runtime contracts;
- a project asset pool with provenance, exact decoded metadata, missing-media handling, duplicate discovery, and deterministic relinking;
- a typed AIR graph for source spans, auditory objects, polyphonic pitch trajectories, parameters, automation, modulation, transforms, relations, evidence, provenance, and competing hypotheses;
- multiresolution constant-Q, multipitch, convolutional-NMF, and multiband rhythm-deprojection engines whose results remain explicitly hypotheses rather than recovered truth.
- deterministic reconstruction proposals that preserve anonymous hit families, competing pitch/modulation implementations, microtiming, full-source residuals, evidence, confidence, and caveats;
- built-in polyphonic subtractive synthesis and sample playback with exact scheduling, bounded voices, and allocation-free block rendering after construction;
- a versioned portable project envelope with atomic saves, autosave recovery, asset-route intent, validation, and explicit domain-codec boundaries;
- a provenance-pinned offline model registry and worker boundary: installed artifacts are hash-verified, and missing models remain an explicit user-download state.

## Getting started

audec currently targets macOS. GPUI uses Apple’s Metal stack, so you will need a current stable Rust toolchain and Xcode Command Line Tools.

```sh
git clone https://github.com/emberian/audec.git
cd audec
cargo run --release -- "/path/to/material.flac"
```

The analysis path currently accepts FLAC. You can also launch without a path and choose **Open audio…** or press `⌘O`.

macOS may reject direct command-line access to files in Desktop, Documents, or other protected folders. Opening the file through audec’s native picker grants the appropriate access for that run.

To build a local app bundle:

```sh
cargo build --release
scripts/bundle-macos.sh release
open target/Audec.app
```

The bundle identifier is `software.ember.audec`.

## Interaction reference

### Workbench

| Input | Action |
| --- | --- |
| `Space` | Play or pause |
| `←` / `→` | Seek backward or forward five seconds |
| `⌘O` | Open FLAC material |
| `⌘1` | Open Waterfall |
| `⌘2` | Open Rhythm and recurrence |
| `⌘3` | Open recurring Components |
| `⌘4` | Open HPSS Decomposition around the playhead |
| `⌘5` | Open Loom around the playhead |
| `⌘6` | Open the arrangement editor |
| `⌘7` | Open the piano-roll / drum sequencer |
| `⌘8` | Open the mixer |
| `⌘9` | Open the automation editor |
| Click a plot | Seek within that plot’s visible range |
| Drag a plot | Select an exact sample range |
| `=` / `-` | Zoom the arrangement in or out around the playhead |
| `Shift-←` / `Shift-→` | Pan independently of playback |
| Mouse wheel / `Shift`-wheel | Pan the arrangement horizontally |
| `0` | Fit the whole material |
| `F` | Re-enable playhead follow |
| `⌘L` | Set and enable the loop from the selection |
| `L` | Toggle the loop |

### Popout lenses

| Input | Action |
| --- | --- |
| `Space` | Play or pause the shared transport |
| `←` / `→` | Seek backward or forward five seconds |
| `=` / `-` | Zoom the local time viewport in or out |
| `Shift-←` / `Shift-→` | Pan the local time viewport |
| Mouse wheel or trackpad scroll | Pan the local viewport without moving playback |
| `0` | Fit the whole material |
| **Follow** | Center on the playhead and resume following it |

Manual pan or zoom disengages follow mode. In Waterfall, `FFT−` / `FFT+` and **Win** recompute the spectrum; `F−` / `F+` change the frequency viewport; and `D−` / `D+` and `R−` / `R+` change the dB ceiling and range from retained numeric values. In Decomposition, **Analyze view** reruns HPSS for the visible span. In Loom, the audition controls compare the source mix, reconstruction, residual, and selected template.

## Architecture

audec keeps numerical work, project truth, and GPU presentation separable:

```text
FLAC source
  └── retained PCM + waveform pyramid
      ├── whole-material measurements and spectral field
      ├── pulse, onset, recurrence, and NMF hypotheses
      ├── selection-local complex STFT + HPSS
      └── recurrence events → Loom templates → render + residual

Project session + Auditory IR
  └── GPUI workbench and synchronized native lens windows
```

The main implementation areas are:

| Path | Responsibility |
| --- | --- |
| `src/ui.rs` | GPUI workbench, transport UI, popout lenses, and visual interaction |
| `src/analysis.rs` | FLAC decoding, whole-material measurements, spectral projection, rhythm and recurrence hypotheses |
| `src/pyramid.rs` | Retained PCM and multiresolution waveform queries |
| `src/decomposition.rs` | Deterministic nonnegative matrix factorization |
| `src/hpss.rs` | Phase-preserving STFT, soft-mask HPSS, inverse synthesis, and null measurement |
| `src/loom.rs` | Recurrence-template inference, editable events, overlap-add rendering, and fit metrics |
| `src/session.rs` | Sample-accurate arrangement state, stable IDs, selection, snapping, and undo/redo |
| `src/arrangement.rs` | Persistent typed tracks/clips and non-destructive DAW edit transactions |
| `src/arrangement_view.rs` | Native exact-frame arrangement editor |
| `src/sequencer.rs` | Tempo/meter map, piano-roll and step patterns, and exact event scheduling |
| `src/sequencer_view.rs` | Piano-roll and drum/step editing UI |
| `src/automation.rs` | Typed parameter targets and immutable realtime automation compilation |
| `src/mixer.rs` | Routing, buses, sends, inserts, latency, and mixer edit history |
| `src/control_views.rs` | Mixer and automation editing UI |
| `src/assets.rs` | Project media pool, provenance, missing media, search, and relinking |
| `src/asset_view.rs` | Searchable media-pool UI with provenance and usage inspection |
| `src/audio.rs` / `src/audio_host.rs` | Sample-exact transport plus independent project and audition buses |
| `src/render.rs` | Deterministic offline rendering, residual checks, and WAV export |
| `src/daw_render.rs` / `src/daw_engine.rs` | Immutable aggregate render schedules and audible project rendering |
| `src/instruments.rs` | Built-in sampler and polyphonic subtractive synthesizer DSP |
| `src/plugin.rs` | Safe plugin discovery, state, ports, automation, and process-boundary contracts |
| `src/rhythm.rs` | Multiband onset, tempo ambiguity, beat/downbeat, hit-family, and pattern evidence |
| `src/cqt.rs` / `src/pitch.rs` / `src/nmfd.rs` | Constant-Q, pitch/modulation evidence, and recurring component hypotheses |
| `src/ontology.rs` | Typed AIR objects, pitch, transforms, modulation, evidence, provenance, and alternatives |
| `src/reconstruction.rs` | Ranked editable reconstruction proposals with uncertainty and residuals |
| `src/project_io.rs` | Portable project envelopes, atomic saves, autosave, and recovery |
| `src/model_registry.rs` / `src/model_worker.rs` | Verified optional-model artifacts and isolated worker protocol |
| `src/timeline.rs` | Transport-independent sample-coordinate viewport mechanics |
| `src/spectral_tiles.rs` | Visible-range, pixel-aware spectral analysis and bounded tile caching |
| `src/workspace.rs` | Stable dock/tab/tear-off layout model and persistence |
| `src/settings.rs` | Typed analysis and lens parameters, including retained settings from the original audec |

The original SDL/PortAudio views remain under `src/view/` as reference implementations while their useful controls and behavior are rebuilt as typed GPUI lenses. They are not part of the current application target.

For the complete epistemic model and workspace design, read [docs/VISION.md](docs/VISION.md). The concrete “roughly 85% of LMMS plus reverse-DAW advantages” acceptance gates live in [docs/LMMS_PARITY_ROADMAP.md](docs/LMMS_PARITY_ROADMAP.md). Optional model-worker strategy and audited decomposition candidates are documented in [docs/ML_MODELS.md](docs/ML_MODELS.md).

## Development

```sh
cargo fmt --check
cargo test
```

The test suite covers analysis primitives, waveform resolution, deterministic NMF, reconstructible STFT/HPSS, Loom inference and rendering, settings normalization, sample-coordinate timeline behavior, session history, and AIR graph validation. The development profile intentionally enables modest optimization because whole-track FFT and clustering work is otherwise unrepresentatively slow.

When adding an analysis result, preserve three things:

1. its exact backlink to source samples;
2. the parameters, implementation or model identity, and provenance that produced it;
3. an audible or measurable way to challenge the claim, ideally including its complement or residual.

## Roadmap

- **One audible project** — connect arrangement, patterns, instruments, automation, mixer, transport, offline export, and persistence through the aggregate render engine.
- **Editor unification** — make arrangement, sequencer, assets, mixer, automation, and analysis lenses first-class dockable panes sharing selection, time, and project state.
- **Deeper explanatory decomposition** — interactive NMF/NMFD, masking and reference-guided queries, then provenance-pinned optional ML workers whose outputs remain auditionable hypotheses.
- **Production reconstruction** — turn ranked rhythm, pitch, modulation, sample, synthesis, effects, and routing proposals into editable project branches with continuous residual comparison.
- **DAW completeness** — recording, richer instruments/effects, controller mapping, freeze/bounce, reliable plugin hosting, project recovery, and the workflow gates in the LMMS-parity roadmap.
- **Perceptual instruments** — agency and grouping alternatives, personal descriptive lexicons, and explicit crossmodal/vEAR bindings.

The design test is simple: every mark should answer **what evidence is this, how was it derived, can I hear it, and what happens if I edit it?**

## License

Dual-licensed under [Apache-2.0](LICENSE.APACHE2) or [MIT](LICENSE.MIT), at your option.
