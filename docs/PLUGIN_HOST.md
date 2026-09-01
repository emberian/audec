# Plugin host boundary

audec's plugin model is intentionally smaller than a CLAP, VST3, or Audio
Unit SDK. Project files speak the stable types in `src/plugin.rs`; format
adapters translate at the edge. The first executable scanner is CLAP through
Clack. VST3 and Audio Unit can later be added without migrating project
automation or mixer state.

The current vertical slice includes indexing/persistence plus a synchronous
control-thread supervisor for an isolated runtime worker. It launches a worker
without a shell, bounds JSONL and stderr input, enforces startup/process
deadlines, validates every lifecycle transition, kills a crashed or hung
worker, and can recreate retained instances from their last verified state and
parameter values. A deterministic fake worker exercises protocol scan, instance,
parameter, process, state, crash, timeout, and recovery paths end to end. No
current audec binary maps a third-party plugin into the application process.

`audec-clap-worker` is a separate, real scanner and first DSP adapter. It uses Clack 0.1.1
to load a canonical `.clap` library or macOS bundle, enumerate its factory,
instantiate each descriptor, and query audio ports, note ports, parameters,
state, latency, tail, and offline-render extensions. On macOS and Linux it can
re-fingerprint a granted artifact, instantiate one CLAP ID, attach four bounded
named mappings, activate, deliver sample-offset parameter and CLAP note events,
process f32 PCM, save state, deactivate, and destroy. A source-controlled
MIT/Apache Clack gain plugin proves actual native DSP over the mapping. VST3
and Audio Unit execution remain unsupported; VST3 identity is
preserved by the schema and fake protocol fixture only, while Audio Unit is
persistence-only.

## Trust boundaries

```text
GPUI / project process
  | candidate paths + hard limits
  v
scanner subprocess ----> validated ScanRecord ----> persistent PluginIndex
                                                      |
                                  artifact digest + PluginKey + opaque state
                                                      v
audio graph compiler ---- capability/lease ----> optional runtime worker
                             |                         |
                             +-- bounded shared PCM/event regions
```

- Project parsing reads only plain plugin keys, parameter values, opaque state,
  and cached metadata. It never loads a plugin.
- Artifact discovery reads directory metadata and recognizes `.clap`
  files/bundles. It never descends into a bundle or calls an ABI entry point.
- Installed-plugin refresh is best-effort but explicitly bounded by directory
  depth, filesystem-entry count, candidate count, artifact bytes, descriptor
  count, parameter count, and per-candidate deadline. Missing standard roots
  and unreadable siblings are diagnostics, not reasons to discard healthy
  candidates from the same pass.
- Descriptor extraction happens in a disposable scanner subprocess with a
  timeout, descriptor/parameter caps, one granted path, and no project access.
- Identical bytes that repeatedly crash are quarantined. A user may explicitly
  retry them; changed bytes are eligible for a fresh scan automatically.
- A native runtime request combines an opaque lease with a canonical path and
  scan fingerprint. The child canonicalizes and hashes it again immediately
  before loading; recovery replays the same grant and refuses changed bytes.
  Filesystem sandboxing is still launcher/platform work, so the path grant is
  an identity check rather than a complete OS sandbox.
- Control uses bounded JSONL. PCM and fixed-size event records use four named,
  separately directed mappings from maintained `shared_memory` 0.12.4. The
  controller owns/unlinks them; a worker crash cannot destroy mappings needed
  for recovery. `Process`/`Processed` are the single-slot commit fences.
- The realtime audio callback never discovers, loads, allocates, performs IPC
  setup, saves state, logs, or waits on a worker.

## Identity and provenance

`PluginKey` is `(format, native identifier)`. A path and a scan-order index are
not identities. CLAP parameter automation uses the plugin's native `u32` key;
future VST3 and Audio Unit adapters have native key forms in the same enum.

An `ArtifactFingerprint` records content digest, byte length, and architecture.
Single libraries and complete macOS bundles use the same framed SHA-256
algorithm. Bundle entries are sorted; symlinks, non-UTF-8 relative paths,
empty artifacts, and artifacts above the configured byte cap are rejected.
Every successful or failed scan carries scanner build/version, OS and
architecture, and the digest algorithm. The scan cache replaces records by
canonical path but considers their content fingerprint when deciding whether
the result remains valid. A project instance may additionally pin the artifact
digest it was last known to use.

`PluginStateBlob` is opaque and size-bounded. Its digest is supplied by the
state-storage layer; the core does not invent a weak hash. State, native
parameter keys and their values survive missing, changed, quarantined, crashed,
or unsupported plugins. This lets a project be opened and saved losslessly on
a machine that cannot execute its devices.

## Missing-plugin behavior

Fallback is explicit and persisted:

- a known audio effect bypasses audio;
- an instrument emits silence and no notes;
- an unknown or ambiguous device emits silence.

The graph compiler must validate channel mapping before allowing effect
bypass. A placeholder stays visible with its identity, state, automation,
diagnostic, and relink/rescan affordances. audec never silently substitutes a
different plugin with the same display name.

## DSP contract

Before activation, the graph compiler and adapter agree on sample rate, block
range, exact audio-port layouts and channel slots, note dialects, online versus
offline mode, initial latency, and tail. Audio ports include main, auxiliary,
and sidechain roles. Latency or tail changes are control-plane events that
invalidate the prepared graph or export tail plan; they are not applied by
mutating routing inside a callback.

Automation is sent as bounded, sample-offset events. A published batch must be
sorted, remain inside its half-open block, reference known stable parameter
keys, contain finite normalized values, and contain at most one value for a
parameter at a sample offset. Continuous smoothing is an adapter/device
responsibility declared by the eventual universal parameter registry; stepped
parameters must not be smoothed.

The runtime protocol and supervisor use monotonically increasing process
sequence numbers so stale and missing completions are rejected. A missed
deadline terminates the child and marks the host failed; the caller applies the
instance's declared fallback for that block and schedules `recover` on its
control thread. The supervisor itself is deliberately unsuitable for an audio
callback because its methods perform IPC and bounded waits.
`process_block_or_silence` is the narrow controller hook: on any timeout,
crash, protocol failure, or native error it clears the entire output mapping,
increments a diagnostic counter, and returns a typed silenced outcome. It does
not hide failure by replaying stale samples or substituting another plugin.

Offline rendering uses a private instance and the same processing contract.
`DeterminismClass` prevents audec from promising sample-identical freeze or
cache reuse for plugins driven by wall clock, network, external hardware, or
unrecorded entropy. Seeded plugins must store the effective seed in render
provenance.

## CLAP integration slices

### 1. Scanner executable — implemented

`audec-clap-worker` accepts one validated
`ScanRequest`, hashes the complete bundle, resolves architecture, initializes
the CLAP entry, enumerates the plugin factory, queries static descriptors and
extension-backed ports/parameters, emits one bounded `ScanResponse`, calls
deinit when the entry is released, and remains available for more scan requests.
The parent enforces timeout and records signal/exit status. Invalid native bytes
are rejected by a real subprocess regression test.

### 2. Isolated runtime adapter — first executable slice implemented

The controller creates four contract-sized named mappings. The worker opens
them by a 96-bit POSIX-safe native name derived from each 128-bit wire token,
while collisions fail closed. Audio is planar f32; the event slot has a
versioned fixed record layout and explicit count cap. JSON control owns
ordering, so this is deliberately one bounded block slot rather than a second
realtime ring or scheduler. The runtime handles a single contiguous audio bus
per direction (including zero input for an instrument), sample-accurate
parameter/note events, online/offline render mode, state save/restore, initial
latency, and the persisted tail contract.

Still explicit: multiple audio buses/sidechains, f64 buffers, CLAP output-event
translation, dynamic latency/tail notifications after activation, GUI bridging,
and a hard realtime callback bridge are unsupported. The current supervisor is
synchronous and belongs on the existing engine's control/render worker, never
inside its device callback.

### 3. Instruments, notes, sidechains, editors

Add note dialect negotiation and scheduled note events, then auxiliary and
sidechain buses. Editor windows come later and must obey CLAP main-thread calls
without granting the plugin authority over the workspace model. Editor failure
must not destroy the audio instance or its state.

### 4. Runtime isolation — controller and first transport implemented

The controller already validates lifecycle transitions, bounds control I/O,
enforces deadlines, detects crash/EOF, kills hung children, reports diagnostics,
and replays retained instance recipes after restart. Named POSIX mappings are
qualified on macOS and Linux. Windows scanning remains possible, but the worker
advertises no DSP/shared-memory capability there until its handle lifecycle is
tested. Only after measurement should audec choose per-instance, per-vendor, or
pooled workers.

## Existing DawEngine adoption API

No plugin scheduler or second realtime engine was added. The adoption seam is:

1. `compile_daw_engine` remains the only graph compiler and retains the existing
   `plugin_instruments: BTreeMap<u64, ProcessorId>` routing identity.
2. On the control/render worker, resolve `PluginKey` plus pinned artifact digest,
   form the existing `ProcessingContract`, call
   `plugin_worker::transport::binding_for`, and retain the returned
   `SharedBlockTransport` alongside an `InstanceRecipe`.
3. For each already-scheduled DAW block, publish its planar channels and
   `InputEvent`s with `controller_write_inputs`, call
   `OutOfProcessPluginHost::process_block_or_silence`, then copy successful
   output with `controller_read_outputs`. The `ProcessorId` remains the graph
   node identity; the plugin instance token is adapter-private.
4. Offline bounce creates a private instance with `ProcessingContract.offline`
   and uses the same block adapter. State and the granted artifact digest are
   retained in render identity/provenance by the caller.

## Catalog refresh and insertion planning

`OutOfProcessPluginHost::refresh_clap_catalog` is the UI-neutral production
workflow for an installed-plugin pane. It must run on a scanner-only host (a
host with active DSP instances is refused), and performs one deterministic
pipeline:

1. bounded, symlink-refusing, best-effort discovery across standard and custom
   roots;
2. canonicalization and controller-side full-artifact fingerprinting;
3. cache reuse for unchanged ready entries and unchanged quarantines;
4. deadline-bound scanner IPC using a host-minted request ID;
5. response path/digest verification, failure counting, and quarantine; and
6. worker recovery after a crash or timeout so later candidates still receive
   an independent attempt.

The returned `PluginCatalogRefreshReport` preserves discovery issues and one
typed outcome per attempted candidate. No UI needs to parse stderr to
distinguish cached, scanned, failed, or quarantined artifacts.

`PluginIndex::compatibility_report` then projects that cache for an exact DAW
use case (audio insert or instrument), architecture, isolation mode, sample
rate, block range, online/offline mode, and main-bus layouts. It either emits a
validated `ProcessingContract` plus stable-ID-ordered compatible backends, or
structured refusal reasons such as role mismatch, unavailable layout,
required sidechain, missing CLAP note input, unsupported output events, or
offline incompatibility. `plan_insertion` additionally resolves the native ID
and optional pinned artifact digest, initializes normalized writable parameter
defaults, and creates a lossless persistent placeholder before native launch.

`PluginInstanceControlState::apply_command` is the authoritative post-insert
control reducer. Parameter/state edits and verified runtime latency, tail, and
availability reports carry an expected instance revision and explicit `before`
values. Accepted changes return `PluginRenderInvalidation`, separately marking
processing-graph/latency recompilation, render-content invalidation, output-tail
replanning, and persisted-instance changes. Stale gestures and late worker
notifications fail closed instead of silently mutating a newer instance.

## Deliberate non-features of this slice

- No dynamic library loading or ABI parsing in the application process.
- No claim that a discovered artifact is valid before a scanner record exists.
- No VST3/AU SDK type in persisted state.
- No pointer-derived parameter identity.
- No unbounded state, descriptor, event, or shared-memory allocation.
- No assumption that plugin output is deterministic, realtime-safe, or
  available offline merely because the vendor advertises it.

## Primary references

- [CLAP specification and headers](https://github.com/free-audio/clap),
  especially the factory, plugin, process, parameter, state, audio-port,
  note-port, latency, tail, and render contracts.
- [Clack host implementation](https://github.com/prokopyl/clack), used at
  version 0.1.1 under MIT OR Apache-2.0. Native loading is still documented by
  Clack as inherently unsafe, which is why it appears only in the worker.
- [CLAP reference host](https://github.com/free-audio/clap-host), used to
  confirm the macOS bundle location and note-event conventions.
- [`shared_memory`](https://github.com/elast0ny/shared_memory), used at version
  0.12.4 for maintained POSIX/Windows mapping primitives. Audec currently
  qualifies and advertises the DSP path only on macOS and Linux.
