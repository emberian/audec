# Plugin host boundary

audec's plugin model is intentionally smaller than a CLAP, VST3, or Audio
Unit SDK. Project files speak the stable types in `src/plugin.rs`; format
adapters translate at the edge. The initial executable adapter should be CLAP
through Clack. VST3 and Audio Unit can then be added without migrating project
automation or mixer state.

The current vertical slice is real indexing and persistence infrastructure,
not executable hosting. It can discover `.clap` artifacts without loading
them, validate scanner output, maintain a deterministic cache and quarantine,
preserve opaque state and automation for missing plugins, negotiate ports, and
describe the scanner/runtime process protocols. No current audec binary maps a
third-party plugin into memory.

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
- Descriptor extraction happens in a disposable scanner subprocess with a
  timeout, descriptor/parameter caps, one granted path, and no project access.
- Identical bytes that repeatedly crash are quarantined. A user may explicitly
  retry them; changed bytes are eligible for a fresh scan automatically.
- A runtime worker receives an opaque artifact lease rather than an arbitrary
  filesystem path. It exchanges control messages over IPC and bounded audio
  and event slots through separately directed shared-memory regions.
- The realtime audio callback never discovers, loads, allocates, performs IPC
  setup, saves state, logs, or waits on a worker.

## Identity and provenance

`PluginKey` is `(format, native identifier)`. A path and a scan-order index are
not identities. CLAP parameter automation uses the plugin's native `u32` key;
future VST3 and Audio Unit adapters have native key forms in the same enum.

An `ArtifactFingerprint` records content digest, byte length, and architecture.
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

The runtime protocol includes monotonically increasing process sequence
numbers so stale and missing completions can be detected. The host must define
a fixed deadline/failure policy before using an external worker in realtime.
An underrun never blocks the callback: it produces the instance's declared
fallback, increments a visible diagnostic, and schedules recovery off-thread.

Offline rendering uses a private instance and the same processing contract.
`DeterminismClass` prevents audec from promising sample-identical freeze or
cache reuse for plugins driven by wall clock, network, external hardware, or
unrecorded entropy. Seeded plugins must store the effective seed in render
provenance.

## CLAP integration slices

### 1. Scanner executable

Add a small `audec-plugin-scan` binary using Clack. It accepts one validated
`ScanRequest`, hashes the complete bundle, resolves architecture, initializes
the CLAP entry, enumerates the plugin factory, queries static descriptors and
extension-backed ports/parameters, emits one bounded `ScanResponse`, calls
deinit, and exits. The parent enforces timeout and records signal/exit status.
Golden tests should include valid multi-plugin bundles, malformed strings,
duplicate IDs, enormous counts, missing callbacks, hangs, aborts, and a plugin
whose descriptor changes between scans.

### 2. Trusted in-process effect

Add an audec-owned adapter trait and a Clack implementation for a stereo audio
effect with no editor. Instantiate and activate off-thread, process from the
same prepared graph online and offline, support sample-offset parameters and
opaque save/restore, publish latency/tail changes, and retain the existing
missing placeholder on any failure. Validate against a tiny source-controlled
CLAP test plugin before scanning user directories.

### 3. Instruments, notes, sidechains, editors

Add note dialect negotiation and scheduled note events, then auxiliary and
sidechain buses. Editor windows come later and must obey CLAP main-thread calls
without granting the plugin authority over the workspace model. Editor failure
must not destroy the audio instance or its state.

### 4. Runtime isolation

Implement the existing worker messages with OS handles for the four directed
shared regions, lifecycle state-machine validation, bounded SPSC rings,
deadlines, heartbeat/crash detection, restart and state recovery. Only after
measurement should audec choose per-instance, per-vendor, or pooled workers.
The protocol already avoids exposing project paths and keeps later remote DSP
or architecture-translation experiments out of project schema.

## Deliberate non-features of this slice

- No dynamic library loading or ABI parsing in the application process.
- No claim that a discovered artifact is valid before a scanner record exists.
- No VST3/AU SDK type in persisted state.
- No pointer-derived parameter identity.
- No unbounded state, descriptor, event, or shared-memory allocation.
- No assumption that plugin output is deterministic, realtime-safe, or
  available offline merely because the vendor advertises it.
