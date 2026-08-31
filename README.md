# audec

**A reverse DAW for taking recorded sound apart without pretending the evidence is more certain than it is.**

![audec arrangement workbench](docs/screenshots/audec-arrangement.png)

audec is a native, multiwindow audio workbench built with Rust and GPUI. Give it a finished recording and it turns the mix into inspectable measurements, recurring-pattern hypotheses, editable event templates, audible reconstructions, and residuals. The goal is not to claim access to the lost original session. It is to build the most useful explanation of the sound that the evidence supports—and let you listen to where that explanation fails.

> **Project status:** audec is an early macOS prototype under active development. The current application is useful for exploring FLAC material and testing the reverse-DAW workflow, but it is not yet a production editor or source-separation product.

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

The current GPUI application provides a synchronized whole-material workbench and five native popout lenses:

- **Workbench atlas** — stereo waveform, log-frequency energy, loudness, spectral centroid, transient, stereo-width and correlation lanes, plus measurements at the shared playhead.
- **Waterfall** — a retained numeric dB field with independent time and log-frequency viewports, adjustable FFT size, window function, dB ceiling, and display range. Analysis settings recompute the field instead of merely stretching an image.
- **Rhythm and recurrence** — an explicitly provisional pulse hypothesis, low/mid/high onset evidence, and onset-centered spectral recurrence clusters with source times and similarity measurements.
- **Components** — deterministic NMF spectral templates and activation histories presented as recurring mixed-audio hypotheses, with fit information rather than instrument names.
- **Decomposition** — reconstructible, selection-local HPSS with audible original, tonally sustained estimate, transient estimate, and mixture null. Analysis is currently capped at 30 seconds while tiled complex-STFT caching is unfinished.
- **Loom** — aligned, phase-preserving excerpts from recurring attacks deprojected into an editable event sequence. Clusters and events can be muted, shifted, or gain-adjusted; audec immediately overlap-adds a new render and exposes the residual.

All lenses share playback and seeking while retaining their own view range. They can be paused on a detail, panned independently of transport, zoomed, and asked to follow the playhead again. The waveform is backed by a multiresolution min/max/RMS pyramid over retained PCM, so zooming requests new source detail rather than enlarging a whole-song thumbnail.

Loom is deliberately described as **event-template resynthesis**, not recovered stems. Its templates are excerpts from the mixture and may contain overlapping voices, room sound, and effects. NMF likewise finds recurring structure in a nonnegative time-frequency field; it does not establish source identity.

The repository also contains UI-independent foundations now being integrated into the application:

- a sample-accurate project session with typed track, lane, clip, cluster, and event IDs; transport, loop, selection, snapping, revisions, and reversible commands;
- a typed AIR graph for source spans, auditory objects, polyphonic pitch trajectories, parameters, automation, modulation, transforms, relations, evidence, provenance, and competing hypotheses;
- sample-coordinate viewport mechanics for future arrangement and lens timelines.

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
| Click a plot | Seek within that plot’s visible range |

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
| `src/ontology.rs` | Typed AIR objects, pitch, transforms, modulation, evidence, provenance, and alternatives |
| `src/timeline.rs` | Transport-independent sample-coordinate viewport mechanics |
| `src/settings.rs` | Typed analysis and lens parameters, including retained settings from the original audec |

The original SDL/PortAudio views remain under `src/view/` as reference implementations while their useful controls and behavior are rebuilt as typed GPUI lenses. They are not part of the current application target.

For the complete epistemic model, workspace design, and staged implementation plan, read [docs/VISION.md](docs/VISION.md). The optional model-worker strategy and audited source-separation candidates are documented in [docs/ML_MODELS.md](docs/ML_MODELS.md).

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

- **Arrangement editing** — connect the project session to a zoomable, scrollable multitrack timeline with selections, loops, snapping, stable event manipulation, and undo/redo.
- **Flexible workspaces** — split/tab layouts, saved workspaces, linked view facets, and reliable tear-off/dock-back across native windows.
- **Stronger rhythm deprojection** — SuperFlux-style multiband novelty, tempograms, visible half/double-time alternatives, editable beats, meter hypotheses, and beat-synchronous pattern starts.
- **Deeper explanatory decomposition** — interactive NMF/NMFD, masking and reference-guided queries, then optional provenance-pinned ML workers whose outputs remain auditionable hypotheses.
- **Production reconstruction** — non-destructive clips, automation and modulation lanes, source/gesture models, effects and routing hypotheses, plugin-ready rendering, export, and continuous residual comparison.
- **Perceptual instruments** — agency and grouping alternatives, personal descriptive lexicons, and explicit crossmodal/vEAR bindings.

The design test is simple: every mark should answer **what evidence is this, how was it derived, can I hear it, and what happens if I edit it?**

## License

Dual-licensed under [Apache-2.0](LICENSE.APACHE2) or [MIT](LICENSE.MIT), at your option.
