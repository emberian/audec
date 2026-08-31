# audec

**audec** is an audio decompiler: a native, multiwindow workbench for turning a fixed recording into inspectable evidence and revisable hypotheses.

The current GPUI prototype loads a FLAC, plays it, and constructs a whole-material atlas containing:

- stereo waveform and field measurements;
- a log-frequency spectral overview;
- loudness, spectral-centroid, transient, and stereo feature lanes;
- a pulse hypothesis with explicitly named contrast/support;
- low/mid/high onset evidence;
- onset-centered spectral recurrence clustering with named similarity evidence;
- a deterministic NMF lens for recurring mixed-audio spectral/activation hypotheses;
- selected-span reconstructible HPSS with audible sustained/transient estimates and a measured mixture null;
- an editable **Loom** that infers aligned reusable event templates, deprojects their occurrence times and gains, overlap-adds a new render, and exposes the unexplained residual;
- synchronized native Waterfall, Event recurrence, Components, Decomposition, and Loom windows with independent time and frequency viewports;
- a PCM-backed multiresolution waveform pyramid, retained numeric dB field, live Waterfall dB transfer, FFT-size, and window-function controls.

Event clusters are deliberately not labeled as instruments. They say “these mixed-audio attacks have similar spectral shapes,” preserve their source times, and show their mean template similarity. They do not prove sample identity. NMF likewise produces recurring component hypotheses, not stems. HPSS separates time-persistent from frequency-broad evidence over a selected span and exposes the reconstruction null. Loom is the first construction loop: its templates are real, aligned excerpts from the mix and its timing/gain/mute edits rerender PCM immediately. Because the excerpts still contain overlapping voices and effects, this is event-template resynthesis rather than recovered original stems. NMFD, ML model workers, true CQT, structure recurrence, and production-chain inference remain work in progress.

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
- `⌘2`: open a synchronized Event recurrence / pulse hypotheses lens
- `⌘3`: open recurring NMF component hypotheses
- `⌘4`: decompose an 18-second Aspect around the playhead
- `⌘5`: infer and open an editable event-template reconstruction around the playhead

Detached lenses:

- `+` / `−`: zoom the time viewport around the playhead
- `Shift-←` / `Shift-→`: pan the viewport
- horizontal or vertical wheel/trackpad scrolling: pan the lens viewport without moving playback
- `0`: fit the whole material
- header `Follow`: center on the shared playhead and follow it when it leaves the view; any manual pan/zoom disengages follow
- header `F+` / `F−`: zoom the Waterfall’s log-frequency viewport
- Waterfall `FFT−` / `FFT+` and `Win`: rerun the numeric spectrum from retained PCM with a different FFT size/window
- Waterfall `D−` / `D+` and `R−` / `R+`: adjust dB ceiling/range from retained numeric values
- Decomposition `Analyze view`: rerun HPSS for the current span (currently capped at 30 seconds pending tiled STFT caching)
- Decomposition audition buttons: compare original, sustained estimate, transient estimate, and mixture null
- Loom controls: select/mute/gain a recurrence cluster; move, gain, or disable its event nearest the playhead; audition the source, render, residual, or selected template
- clicking a plot seeks the shared transport within that lens’s visible range

The whole-song workbench is an atlas, not the only scale. Waveform lenses now query retained PCM at the visible resolution, and Waterfall method changes rerun analysis rather than stretching a bitmap. The old SDL audec’s remaining waveform shaping, zero-crossing alignment, capture controls, and vectorscope persistence are represented in typed settings and remain requirements as their dedicated lenses are rebuilt.

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

The model-worker strategy and audited electronic-music candidates are in [docs/ML_MODELS.md](docs/ML_MODELS.md). ML outputs will enter audec as versioned, auditionable Claims with weight hashes, provenance, complement/residual behavior, and explicit licensing—not as unquestioned original stems.

## License

Dual-licensed under [Apache-2.0](LICENSE.APACHE2) or [MIT](LICENSE.MIT), at your option.
