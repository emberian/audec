# Rust ecosystem adoption map

Status: architecture decision record, audited 2026-08-31. Crate versions are
the latest non-prerelease versions visible in crates.io metadata on that date,
unless a deliberately older compatible version is named.

This is not a request to replace Audec's architecture with a crate-shaped
architecture. Community crates may implement a codec, numerical kernel, OS
adapter, bounded transport, or test oracle. They do not own the project,
command history, render graph, publication boundary, worker protocol, or
provenance.

## Non-negotiable adoption gates

Every adoption must preserve these laws:

1. `DawEngineSchedule` remains the only sound-producing project engine.
   Playback, audition, comparison, and export consume the same immutable
   `RenderPlan`/`RenderProduct` lineage.
2. Durable user edits enter through `CommandBatch`/`DomainCommand`. A device
   callback, file watcher, dialog, worker, or library callback may report an
   observation or intent; it may not mutate the project directly.
3. A result is publishable only with Audec-owned identity and provenance:
   source digest/span, exact crate and algorithm version, effective options,
   input/output format, latency/tail, determinism grade, and output digest.
4. The audio callback performs no allocation, locking, logging, filesystem
   access, process I/O, or unbounded work. Construction, resizing, graph edits,
   and destruction happen outside it.
5. Durable JSON/envelope schemas remain explicit and versioned. Rust layouts,
   third-party IDs, pointer values, channel events, and watcher event ordering
   are never durable identity.
6. Unknown project sections and external-media references remain opaque and
   round-trippable. An adapter must refuse unsupported input rather than
   silently normalize, resample, discard, or invent state.
7. All optional native backends must build behind Cargo features and have a
   deterministic, offline test path. A feature may add capability, never alter
   the meaning of an existing project without a recipe/version change.

The most important existing seams are `MediaDecoder` in
`src/media_resolver.rs`, `ExecutableRenderPlan` and `RenderRuntime` in
`src/render_runtime.rs`, `SharedBlockTransport` in `src/plugin_transport.rs`,
`OutOfProcessPluginHost` in `src/plugin_worker.rs`, `Envelope` in
`src/plugin_wire.rs`, `ProjectFileActions` in `src/file_actions.rs`,
`DeterministicRuntimeCommandCodec` in `src/runtime_command_codec.rs`, and
`WorkspaceSemanticTree` in `src/workspace_accessibility.rs`.

## Decision summary

| Area | Adopt now | Evaluate behind adapter | Reject for now |
| --- | --- | --- | --- |
| Decode | Symphonia as the one import decoder, coordinated with Rodio | FFmpeg only for a future quarantined unsupported-format worker | More parallel per-format decoders |
| Resample/stretch | Rubato for explicit sample-rate conversion | Signalsmith Stretch for pitch-preserving offline stretch | Implicit conversion; stretching in the callback before validation |
| DSP graph | Small `dasp_*` primitives only when they delete audited local math | FunDSP for isolated effect prototypes | FunDSP/`dasp_graph` as a second project graph or scheduler |
| In-process RT transport | `rtrb` SPSC queues | None until measured | Generic MPMC channels on the audio callback |
| Cross-process IPC | Keep `shared_memory` bounded slots plus versioned JSONL control | `interprocess` local sockets if stdio lifecycle becomes limiting | Serializing PCM/events through JSON or an async runtime |
| MIDI | `midir` + `wmidi` for MIDI 1 device I/O and typed packet validation | MIDI 2/UMP only after an owned event schema exists | Persisting backend port handles or mutating notes in callbacks |
| Plug-ins | Keep Clack/CLAP in the isolated worker | `vst3` bindings in the same worker protocol | In-process third-party DSP; old VST2 crates; plugin frameworks as hosts |
| Dialog/watch | `rfd` for user-picked paths; stable `notify` 8.x for hints | Poll fallback for network/FUSE and overflow recovery | Watch events as authoritative project/catalog changes |
| Jobs | GPUI executors and current supervised workers | Rayon in pure offline kernels with ordered publication | App-wide Tokio; Flume or Rayon on the RT path |
| Serialization/diff | Keep Serde/JSON; `similar` only for diagnostics/tests | `rkyv` for validated disposable IPC after benchmarks | JSON Patch as commands; binary Rust layouts as durable files |
| Accessibility | Keep and complete Audec semantic trees | AccessKit only through a GPUI/native-window bridge | A second window event loop or pixel-derived semantics |
| Linux/platform | Keep Rodio/CPAL path; `cargo-packager` as release CLI | Direct CPAL/PipeWire backend after device/latency evidence | Direct PipeWire plus CPAL/Rodio concurrently; a second window toolkit |
| Verification | Proptest, cargo-fuzz, Loom, Criterion as dev/CI tools | Sanitizers and Miri in scheduled CI | Timing benchmarks as correctness or determinism evidence |

## Audio decode, resampling, and time stretch

### Adopt now: Symphonia at `MediaDecoder`

Use [Symphonia 0.6.1](https://crates.io/crates/symphonia/0.6.1) as the single
canonical importer behind `MediaDecoder::decode`. It is pure safe Rust,
feature-modular, fuzz-oriented, and supports the formats Audec needs plus a
clear expansion path. Its repository documents codec/container maturity rather
than claiming uniform support, which fits Audec's refusal model
([upstream capabilities](https://github.com/pdeljanov/Symphonia)). License is
MPL-2.0; keep it as an unmodified dependency or track notices/source changes if
Audec ever carries a fork.

Exact seam and policy:

- Add an `audec_media` adapter implementing `MediaDecoder`; return canonical
  interleaved finite `f32` plus the exact native sample rate/channel count.
- Decode and hash once during `ProjectRepository::hydrate_media`; analysis,
  preview, and rendering reuse the hydrated `PcmAsset` rather than reopening
  the source through different libraries.
- Imported assets whose native rate differs from the project rate persist two
  deliberately separate identities: the encoded source metadata/fingerprint
  used for reopen and relink, and the canonical finite project-rate PCM
  fingerprint used by the renderer. The saved Rubato recipe includes backend
  version, filter/window/interpolation parameters, alignment/trim facts, and
  exact frame counts; hydration recreates it once and refuses output whose
  metadata, recipe, or canonical PCM fingerprint differs.
- Pin explicit Symphonia codec/format features. Do not enable `all` by default.
- Maintain corrupted/truncated/container-bomb fixtures and cross-version PCM
  goldens. A decoder upgrade changes the provenance recipe even if samples are
  byte-identical.

Audec currently receives Symphonia 0.5.5 transitively from Rodio 0.22.2. Adding
0.6.1 immediately would compile two incompatible Symphonia lines. The
dependency lane should either expose Rodio's matching version or coordinate a
Rodio/Symphonia upgrade before landing the adapter. `claxon` should be removed
only after identical FLAC metadata, truncation, and PCM tests pass. This is an
architectural adopt-now decision, not permission for duplicate decoders.

### Adopt now: Rubato at an explicit conversion recipe

[Rubato 5.0.0](https://crates.io/crates/rubato/5.0.0) is MIT OR Apache-2.0,
Rust-native, and offers asynchronous sinc and synchronous FFT resamplers. Its
documented `process_into_buffer` path performs into caller-owned preallocated
buffers and is intended for real-time use, while whole-clip helpers explicitly
handle startup delay and tail
([real-time and whole-clip guidance](https://github.com/HEnquist/rubato)).

Define an Audec-owned `SampleRateConverter` adapter used by explicit import,
offline render/export conversion, and model preprocessing recipes. It must
report algorithm, quality parameters, chunk size, input/output rates, delay,
trim, crate version, and output digest. Prefer `default-features = false` for a
polynomial-only path when FFT quality is unnecessary; the default FFT feature
adds `realfft`/`num-complex` (Audec already has `rustfft`). For live conversion,
construct and size the resampler off-thread and expose only an
allocation-free, fixed-capacity call.

Rubato must not silently change `DawEngineSchedule`'s existing rational clip
resampling. First use it where current code honestly refuses conversion
(`render.rs`, model preprocessing, explicit import). Replacing engine math
requires numerical goldens, latency accounting, and a new engine-recipe stamp.

### Evaluate behind adapter: Signalsmith Stretch

[Signalsmith Stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch/blob/main/README.md)
is a mature MIT C++ pitch/time algorithm with chunked computation and explicit
latency/flush behavior. The current Rust crate
[`signalsmith-stretch` 0.1.3](https://crates.io/crates/signalsmith-stretch/0.1.3)
is a small MIT wrapper around the C++ source, so the wrapper's maintenance,
compiler toolchain, submodule/build reproducibility, sanitizer coverage, and
macOS/Linux/Windows binaries need validation.

Put it behind a `PitchPreservingTimeStretch` trait invoked only while building
an immutable render product. The adapter owns buffers, flush/latency trimming,
ratio limits, formant settings, and provenance. Do not call it from the device
callback until an allocation audit and long-running variable-block stress test
prove that exact wrapper/configuration safe. Keep a deterministic reference
corpus because floating-point output may vary with compiler/FFT acceleration.

## DSP graphs and primitives

### Adopt selectively: `dasp_*` primitives

The [DASP](https://github.com/RustAudio/dasp) family is MIT OR Apache-2.0,
modular, dependency-free for its fundamentals, and documents no dynamic
allocation except selected bus/graph features. `dasp_sample`, `dasp_frame`, or
a small window/envelope primitive may replace duplicated conversion code after
golden comparison. Add individual crates, never the umbrella feature set.

### Reject for now: a community project graph

[FunDSP](https://github.com/SamiPerttu/fundsp) is capable: typed static graphs,
dynamic networks, deterministic seeded generators, preallocation, and an RT
frontend/backend split. Those same features overlap directly with
`DawEngineSchedule`, `RenderPlan`, mixer routing, `RenderRuntime`, and
publication. `dasp_graph` has the same architectural overlap and has a history
of lagging graph dependencies. Neither may become the project renderer,
scheduler, mixer truth, or automation authority.

FunDSP may be evaluated inside one leaf effect whose constructor consumes a
fully specified Audec recipe and whose `process_into` surface is allocation
free. The leaf must be indistinguishable from handwritten DSP to the engine:
no library node IDs in project files and no library graph edits from the UI.

## Lock-free and shared-memory IPC

### Adopt now: `rtrb` for in-process SPSC edges

[`rtrb` 0.4.0](https://crates.io/crates/rtrb/0.4.0) is a small MIT OR
Apache-2.0, wait-free SPSC ring designed for real-time use. Use one bounded,
preallocated queue per single-producer/single-consumer edge: MIDI callback to
control/recording ingress, and only measured control-to-audio events that cannot
use an immutable publication swap. Overflow is an explicit counter/policy,
never blocking or allocation. Queue payloads are fixed-size Audec-owned values;
the queue is transport, not state or history.

Do not add both `rtrb` and `ringbuf` without evidence. `ringbuf` 0.5.1 is also
credible and supports direct slice access, but `rtrb` has the narrower RT SPSC
contract Audec needs. Neither crate crosses a process boundary.

### Keep: `shared_memory` plus Audec's bounded slot protocol

`SharedBlockTransport` already uses
[`shared_memory`](https://github.com/elast0ny/shared_memory) for four bounded
OS mappings and uses versioned JSONL `Process`/`Processed` messages as the
ownership fence. Keep that design. The crate maps bytes; Audec must continue to
own magic/version headers, sizes, single-writer rules, access direction,
sequence matching, zero-on-failure, and worker lifecycle. Do not add a lock-free
queue merely because the bytes are shared.

### Evaluate: `interprocess` for the control plane only

[`interprocess` 2.4.3](https://crates.io/crates/interprocess/2.4.3) is 0BSD OR
Apache-2.0 and offers local sockets/named pipes without requiring Tokio.
Evaluate it only if `WorkerProcess`'s stdin/stdout ownership, reconnect, or
Windows lifecycle becomes a measured problem. It may carry the existing
bounded `plugin_wire::Envelope`; PCM and event blocks remain in shared memory.
An async feature/runtime is unnecessary for the current one-worker protocol.

Generic MPMC channels (`flume`, crossbeam channel) are rejected on the audio
callback: they solve a wider problem, may lock/allocate depending on operation,
and obscure overflow ownership. Flume is also in self-described casual
maintenance ([upstream status](https://github.com/zesterer/flume)).

## MIDI

### Adopt now: `midir` plus `wmidi`

[`midir` 0.11.0](https://crates.io/crates/midir/0.11.0) is MIT and supplies
MIDI device/port I/O across ALSA, CoreMIDI, WinMM/WinRT, JACK, Web MIDI, and
Android; its upstream description is explicit about backend differences and
the absence of a high-level parser
([platform matrix](https://github.com/Boddlnagg/midir)).
[`wmidi` 4.0.11](https://crates.io/crates/wmidi/4.0.11) is a small MIT typed
MIDI 1 parser with optional `no_std`.

Add an Audec-owned `MidiInputBackend`/`MidiPortId` adapter. The Midir callback
validates bytes with Wmidi, copies a bounded owned event into `rtrb`, and
returns. The control thread maps backend timestamps through a calibrated audio
clock into sample-frame events, then creates commands through the existing
authority for recording, learn, and durable mappings. Port names/indices and
Midir connection handles are discovery data, not durable identity. Persist a
user-visible matching descriptor and relink state; explicitly diagnose a
missing or ambiguous device.

This is MIDI 1 only. Preserve raw bounded SysEx by digest/artifact reference;
do not force it through fixed short-message types. MIDI 2 UMP and MPE need an
Audec event schema with group, per-note identity, resolution, timestamp source,
and downgrade policy before adopting another parser.

## Plug-in formats

### Keep/adopted: Clack for CLAP

Continue `clack-host`/`clack-extensions` inside `audec-clap-worker`. Clack is a
safe low-level CLAP wrapper under MIT OR Apache-2.0
([upstream](https://github.com/prokopyl/clack)). It should translate every ABI
value to `PluginKey`, `PluginMetadata`, `ProcessingContract`, parameter/note
events, and opaque state before crossing `plugin_wire::Envelope`. Scanning and
DSP stay out of process; crash quarantine and artifact fingerprints remain
Audec policy.

### Evaluate behind the same worker: VST3

[`vst3` 0.3.0](https://crates.io/crates/vst3/0.3.0) provides permissively
licensed generated bindings, not a complete host. Steinberg's VST3 SDK moved to
MIT in 2025 ([official license](https://github.com/steinbergmedia/vst3sdk/blob/master/LICENSE.txt)),
removing the former SDK-license blocker. Evaluate a separate VST3 worker binary
that implements the existing scan/instantiate/process/state protocol and maps
stable class IDs into `PluginKey`. Validate component/controller split,
bus/speaker arrangements, sample-accurate parameter queues, state round trips,
latency/tail changes, UI isolation, and crash behavior across a fixture matrix.

Reject VST2 and abandoned `vst` host crates. Reject NIH-plug as a host
foundation: it is a plugin-authoring framework and would not remove Audec's
host/protocol obligations. No third-party plugin executes in the GPUI or audio
device process.

## File dialogs and filesystem observation

### Adopt now: `rfd` only for path selection

[`rfd` 0.17.2](https://crates.io/crates/rfd/0.17.2) is MIT and provides native
dialogs on macOS/Windows plus XDG portal or GTK choices on Linux. The 0.17
release replaced a large portal async stack with libdbus
([release notes](https://github.com/PolyMeilex/rfd/releases/tag/0.17.0)). Put it
in a tiny application-layer `NativeFileDialog` adapter that returns a path or
cancel. The chosen path then enters `ProjectFileActions`; RFD never opens,
saves, hydrates, relinks, or changes dirty state itself.

Use the asynchronous dialog surface where the GPUI host can safely await it;
never block the render/control thread. Test parent-window behavior, cancellation,
filters, non-UTF-8 paths, GNOME/KDE/wlroots portals, X11, and sandbox grants.
Review feature weight deliberately: default 0.17 features include XDG portal
and Wayland support, while GTK adds native system dependencies.

### Adopt now: stable `notify` as an invalidation hint

Pin stable [`notify` 8.2.0](https://crates.io/crates/notify/8.2.0), not the 9.0
release candidate. Notify maps to inotify, FSEvents/kqueue,
`ReadDirectoryChangesW`, and a polling fallback
([platforms](https://github.com/notify-rs/notify)). Watch plugin directories,
external media, and model registries only to enqueue a debounced rescan. The
rescan sorts paths, restats, fingerprints content, and applies an Audec-owned
result; raw event order/kinds are never provenance or commands.

Overflow, rename ambiguity, editor save patterns, FUSE/network filesystems, and
sleep/wake require full-rescan or polling recovery. A watcher loss must make
state stale/diagnostic, never silently current.

## Async jobs and scheduling

### Keep: GPUI executors and supervised process workers

UI background work already uses GPUI's foreground/background executors.
`ModelTaskService`, `WorkerRuntime`, `OutOfProcessPluginHost`, cancellation
tokens, and immutable render completions already encode the domain lifecycle.
Keep these rather than introducing a second application runtime.

### Evaluate: Rayon for pure offline kernels

[Rayon 1.12](https://github.com/rayon-rs/rayon/blob/main/README.md) is mature,
MIT OR Apache-2.0, and supports owned thread pools. Use it only for pure,
independent tiles/analysis blocks after benchmarks. Provide an Audec-owned pool
with bounded threads; collect results by stable input key and publish in sorted
order. Rayon's documentation notes that side-effect order may differ, so no
filesystem writes, commands, RNG without per-item seeds, floating-point
unordered reductions, or publication may occur inside a parallel iterator.

Reject an app-wide Tokio runtime. Audec does not need a network server, and a
second scheduler complicates shutdown, thread ownership, test determinism, and
GPUI integration. A crate that internally exposes a `Future` does not require
Tokio; prefer executor-neutral futures or a synchronous worker adapter.

## Serialization, durable data, and diffs

### Keep: explicit Serde DTOs and JSON envelopes

Serde/Serde JSON are mature MIT OR Apache-2.0 infrastructure
([upstream](https://github.com/serde-rs/serde)). Keep handwritten DTOs,
version/tag validation, integer/range checks, deterministic collection order,
bounded lengths, unknown-section preservation, and refusal diagnostics.
Serialization derives reduce syntax; they do not define a persistence policy.

`similar` 3.2 may be adopted as a dev/diagnostic-only Apache-2.0 textual diff
for failed golden JSON and command round trips. It must not create commands or
project deltas.

### Reject as project truth: JSON Patch and `rkyv`

[`json-patch`](https://github.com/idubrov/json-patch) correctly implements RFC
6902/7396, but path-level patches lose Audec's typed addresses, revision
preconditions, coalescing rules, allocator claims, domain invariants, and
collaboration provenance. It is not a replacement for `DomainCommand` or the
command journal.

[`rkyv`](https://github.com/rkyv/rkyv) is a maintained MIT zero-copy framework
with optional byte validation. Evaluate it only for disposable, versioned,
length-bounded worker artifacts if profiling proves JSON/control or raw PCM
metadata is material. Never archive Rust memory layouts as project files,
journal records, plugin state wrappers, or collaboration messages. Cross-build
layout/schema evolution and honest unknown-field preservation matter more than
zero-copy access on those boundaries.

## Accessibility

### Evaluate behind the native GPUI bridge: AccessKit

[AccessKit 0.25.0](https://crates.io/crates/accesskit/0.25.0) is MIT OR
Apache-2.0 and maps a stable semantic tree to UI Automation, NSAccessibility,
AT-SPI, Android, and iOS adapters
([architecture/platforms](https://github.com/AccessKit/accesskit)). It is the
right ecosystem direction, but GPUI 0.2.2 does not expose an AccessKit/native
accessibility bridge for Audec's custom-painted elements.

Keep `WorkspaceSemanticTree`, product semantic nodes, stable IDs, focus, state,
and `command_for_semantic_action` as authority. When GPUI exposes a supported
bridge—or a contained native-window adapter can be proven—translate those
nodes to AccessKit and translate AccessKit actions back into existing semantic
actions/commands. Do not maintain a second semantic tree, derive labels from
pixels, or introduce a winit event loop beside GPUI. On Unix, account for the
AT-SPI/D-Bus dependency and test with Orca; also test VoiceOver and Narrator.

## Linux audio, windows, and packaging

### Keep now; evaluate a coordinated CPAL backend

Rodio 0.22.2 already supplies CPAL 0.17.3 and Symphonia 0.5.5. Keep Rodio as the
device/sink boundary while `AudioHost` remains thin and `CohortRenderer`
supplies the sole PCM stream. Direct [CPAL](https://github.com/RustAudio/cpal)
adoption is justified only by concrete requirements such as device selection,
input, stable device IDs, buffer/clock control, underrun diagnostics, or a
professional backend. CPAL supports CoreAudio, WASAPI, ALSA and optional
JACK/PipeWire/PulseAudio, but Linux ALSA development headers are required and
optional backends add their system libraries.

If adopted, replace—not supplement—the Rodio device owner behind an
`AudioDeviceBackend`. The callback gets a preallocated renderer handle and
reports bounded health counters; it does not become an engine. Coordinate CPAL
and Symphonia versions to avoid duplicate major/minor lines.

### Evaluate: PipeWire through CPAL first

[`pipewire` 0.10.1](https://crates.io/crates/pipewire/0.10.1) is maintained MIT
bindings with system `libpipewire`/SPA requirements. Prefer CPAL's feature
adapter first because it retains the cross-platform device seam. Use direct
PipeWire only if Audec needs graph/session metadata, explicit node routing, or
capabilities CPAL cannot expose. Put it behind a Linux-only feature and adapter;
never let PipeWire node IDs enter project identity. ALSA/JACK fallback and
headless offline rendering must remain available.

GPUI owns windows and its event loop. Reject adding winit/SDL/GTK as a second
window authority. Portal/file-dialog support belongs in the RFD adapter, not a
new UI toolkit.

### Evaluate/adopt in release tooling: `cargo-packager`

Use [`cargo-packager` 0.11.8](https://github.com/crabnebula-dev/cargo-packager)
as a pinned CI/CLI tool, not a runtime dependency. It supports macOS app/DMG,
Linux deb/AppImage/Pacman, and Windows NSIS/MSI. Keep packaging configuration
outside application state and produce checksummed artifacts from a locked
release build. Add separate CI smoke tests for desktop integration, portal and
audio/MIDI system dependencies, file associations, icons, licenses, signing,
and install/uninstall. RPM, Flatpak, and sandbox permissions require explicit
additional tooling; cargo-packager does not make them implicit.

## Testing, fuzzing, and performance

### Adopt now as development-only dependencies/tools

- [Proptest 1.11](https://github.com/proptest-rs/proptest) (MIT OR Apache-2.0):
  generate valid and near-valid commands, DTOs, layouts, journal frames,
  timestamps, frame spans, and plugin events; persist minimized regressions.
  Pin explicit seeds in CI where replay equality matters.
- [cargo-fuzz/libFuzzer](https://github.com/rust-fuzz/cargo-fuzz) (MIT OR
  Apache-2.0 tooling; libFuzzer license also applies): fuzz all untrusted byte
  boundaries—project JSON, command envelopes, journal tails, WAV/media import,
  JSONL worker messages, shared-event slots, plugin descriptors, and state
  headers. It requires nightly, LLVM sanitizer support, and Unix-like x86-64 or
  AArch64; retain portable regression tests for every finding.
- [Loom 0.7](https://github.com/tokio-rs/loom) (MIT): model the atomic
  publication swap, cancellation, bounded SPSC ownership, and supervisor state
  machines behind `cfg(loom)`. Loom explores many C11 interleavings but
  documents memory-model gaps, so it complements rather than proves safety.
- [Criterion 0.8](https://github.com/criterion-rs/criterion.rs) (MIT OR
  Apache-2.0): benchmark decode, resample/stretch, render blocks, command codec,
  journal recovery, and IPC copies with default plotting/Rayon features
  disabled unless needed. Report throughput, tail latency, allocations, and
  underruns separately.

Required properties before each relevant adoption:

- decode/resample/stretch: no panic on arbitrary bounded input; finite PCM;
  exact frame/channel contract; deterministic digest or explicitly graded
  variance; latency and tail accounted;
- command/persistence: `decode(encode(x)) == x`, fresh-codec process restart,
  unknown/refusal preservation, truncation at every byte, bounded recovery;
- realtime/IPC: no allocation after preparation, explicit overflow, stale
  sequence rejection, zeroed failure output, worker crash/restart, and Loom
  ownership models;
- watchers/jobs: duplicate, reordered, dropped, and overflowed observations
  converge after a deterministic rescan; canceled/stale completions cannot
  publish;
- platform: offline render tests are device-independent, plus an opt-in matrix
  for macOS arm64/x86_64, Windows, Linux X11/Wayland, ALSA/PipeWire/JACK,
  portals, MIDI, and assistive technologies.

Benchmark wins are adoption evidence, never semantic evidence. A faster crate
still needs golden output, failure behavior, license review, and provenance.

## Sequenced adoption plan

1. Land no new dependency until the dependency owner chooses a single compatible
   Symphonia/CPAL line and records MSRV/license policy.
2. Introduce adapter traits and reference fixtures first: `SampleRateConverter`,
   `MidiInputBackend`, `NativeFileDialog`, and watcher invalidation service.
3. Add Rubato, then Midir/Wmidi + `rtrb`, then RFD/Notify as separate changes.
   Each change is optional-feature gated and includes the properties above.
4. Add dev-only Proptest/fuzz/Loom/Criterion targets independently of runtime
   adoption; seed them with the existing codec, journal, project, render, and
   plugin-wire regression corpus.
5. Prototype Signalsmith Stretch and VST3 only in worker/offline adapters. Do
   not promote them until platform, crash, deterministic-recipe, and fixture
   matrices pass.
6. Evaluate AccessKit when there is a real GPUI/native accessibility handoff;
   evaluate direct CPAL/PipeWire only from measured product requirements.

No source or Cargo change is implied by this audit. The exact application
adoption hooks are: dialog results call `ProjectFileActions`; watcher events
invalidate and rescan `PluginIndex`/media/model registries; MIDI callbacks feed
a bounded ingress that the command authority consumes; offline DSP returns
immutable products to `RenderRuntime`; plugin format workers speak
`plugin_wire::Envelope`; accessibility actions resolve through
`WorkspaceSemanticAction`; packaging runs after the locked release build.
