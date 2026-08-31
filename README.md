# audec

**audec** is an audio decompiler: a native, multiwindow workbench for turning a fixed recording into inspectable evidence and revisable hypotheses.

The current GPUI prototype loads a FLAC, plays it, and constructs a whole-material atlas containing:

- stereo waveform and field measurements;
- a log-frequency spectral overview;
- loudness, spectral-centroid, transient, and stereo feature lanes;
- a pulse hypothesis with visible confidence;
- low/mid/high onset evidence;
- onset-centered spectral clustering into neutral candidate hit families;
- synchronized native Waterfall and Hit-family windows with independent time and frequency viewports.

Candidate families are deliberately not labeled as instruments. They say “these attacks have similar spectral shapes,” preserve their source times, and show how internally consistent the grouping is. Source separation, HPSS/NMF components, true CQT, structure recurrence, and editable deprojection remain work in progress.

## Run it

audec currently targets macOS because GPUI 0.2.2 requires Apple’s Metal toolchain there.

```sh
cargo run --release -- "/path/to/material.flac"
```

Opening a Desktop/Documents path from the command line may be rejected by macOS privacy controls. Use **Open audio…** / `⌘O` inside the app to grant access through the native file picker.

To make a local app bundle:

```sh
cargo build --release
scripts/bundle-macos.sh release
open target/Audec.app
```

The bundle identifier is `software.ember.audec`.

## Controls

Workbench:

- `Space`: play/pause
- `←` / `→`: seek five seconds
- `⌘O`: open material
- `⌘1`: open a synchronized Waterfall lens
- `⌘2`: open a synchronized Hit families / pulse lens

Detached lenses:

- `+` / `−`: zoom the time viewport around the playhead
- `Shift-←` / `Shift-→`: pan the viewport
- `0`: fit the whole material
- header `F+` / `F−`: zoom the Waterfall’s log-frequency viewport
- clicking a plot seeks the shared transport within that lens’s visible range

The whole-song workbench is an atlas, not the only scale. Lens-local zoom and analysis parameters are part of the project’s core design; the old SDL audec’s FFT window, FFT size, dB bias/range, waveform shaping, zero-crossing alignment, vectorscope persistence, and freely resizable windows are requirements to preserve as their GPUI lenses are rebuilt.

## Build and test

```sh
cargo fmt --check
cargo test
```

The development profile uses modest optimization because whole-track FFT and clustering work is otherwise unrepresentatively slow.

## Direction

The architectural north star, epistemic model, MIR pipeline, workspace ownership, and staged roadmap live in [docs/VISION.md](docs/VISION.md). The short version is:

> Every visual is an instrument, every interpretation is a claim, and every claim can be traced back to what was heard.

The original real-time SDL/PortAudio view implementations remain under `src/view/` as reference material while their useful behavior is ported into typed GPUI lenses.

## License

Dual-licensed under [Apache-2.0](LICENSE.APACHE2) or [MIT](LICENSE.MIT), at your option.
