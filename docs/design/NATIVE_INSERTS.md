# Native inserts (cycle 3, lane C3-Inserts) and the CLAP verdict

Resolves the `+ insert` / "active · not rendered" rows of
`UX_EXPOSURE_AUDIT.md` and the C3-Plugins feasibility question in `PLAN.md`.
Facts verified 2026-09-05 at `b4ee6c4`.

## Today

The mixer model already has inserts: `InsertSlot { processor_id, bypassed,
wet }` on every `Bus` (`src/mixer.rs:167,281`), `Processor` with a
`PluginDescriptor` and parameters (`:130,78`), the audio-thread seam
`insert_processing_contracts` returning `InsertExecution::{ExplicitBypass,
HostWetDry { dry, wet }}` (`:542,198`), latency accounting
(`bus_insert_latency`, `:1186`), persistence (`project_codecs.rs:2796`), and
automation addresses for wet and bypass (`automation.rs:392,402`). The render
graph has no insert node: `NativeNode` (`src/compiled_audio_graph.rs:196`)
knows `Gain, Mix, Delay, AudioClip, Instrument, BusFader, Send, Sanitize`;
the bus chain compiles `mix_or_silence → [pre-fader Sanitize] → pre-fader
Sends → BusFader → Sanitize → post-fader Sends` (`:923-975`), and the
reference renderer only *diagnoses* inserts (`daw_render.rs:671-690`:
`PluginBypassedByReferenceRenderer`, `PluginUnavailable`). Since cycle 1 the
strip says "plugin hosting not connected in this build" and labels every
insert "not rendered" (`control_views.rs:63,1084-1086,3220`). That is honest
and useless.

A CLAP host is already a dependency (`clack-host 0.1.1`,
`clack-extensions` with audio-ports, params, render, state, tail) and the
out-of-process worker is written and tested end to end
(`OutOfProcessPluginHost`, `src/plugin_worker.rs:709`;
`tests/clap_worker_process.rs` against `tests/fixtures/clap_gain`). None of it
is in the library's module graph (`src/lib.rs:100` mounts only `plugin`;
the adapter, wire, worker and transport files are `#[path]`-included by the
worker binary and its test). See `PLUGIN_HOST.md`.

## Verdict on CLAP in the render path

Not in this cycle, for two reasons that are not about capability:

1. Tileability. A hosted plugin's state is opaque, so the only honest
   declarations are `SequentialOnly`, which `tile_contract` refuses
   (`compiled_audio_graph.rs:736`), or `Checkpointable` through the CLAP
   state extension, which is unimplemented (`:735`,
   `TileRefusal::CheckpointImplementationPending`). Tiled rendering is how
   playback, audition and export share one kernel; a project with one
   hosted insert would lose all of it.
2. The realtime contract. `realtime_contract()` (`:665`) asserts
   `io_free_process` and `lock_free_process`; an IPC round trip per block
   falsifies both. `ProcessorRefusalReason::{RealtimeUnsafe,
   StateCannotCheckpoint, NonDeterministic}` (`:100`) already name the
   refusal a hosted insert would get.

The realistic first slice for CLAP is offline: export-only hosting through
`Checkpointable` once checkpoints exist, with playback bypassing the insert
and saying so. That is a later cycle's design, not a lane here.

## Target: native effects as inserts

An insert is a `NativeNode::Insert { input, effect, wet, automation }`
inserted between `mix_or_silence` and the pre-fader `Sanitize`, exactly where
`SendTap` documents inserts ("after inserts, before gain and pan",
`mixer.rs:66`). `effect` is an in-tree DSP kind with parameters:

```rust
pub enum NativeEffect {
    /// State-variable filter (`instruments.rs:1235` is the kernel).
    Filter { mode: FilterMode, cutoff_hz: f32, resonance: f32 },
    /// One-pole high/low shelf and a peaking band: three biquads.
    Eq { low_shelf_db: f32, peak_hz: f32, peak_db: f32, peak_q: f32, high_shelf_db: f32 },
    /// Feed-forward compressor with lookahead 0 (no lookahead in cycle 3).
    Compressor { threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32, makeup_db: f32 },
}
```

The `PluginDescriptor.format` for these is `"native"`; `identifier` names the
kind; parameters live in the existing `Processor.parameters` map so
`ParameterAddress::Plugin { processor_id, key }` addresses them without a
new address family, and `MixerTarget::InsertWet / InsertBypass` become
rendered (whitelist in `address_is_rendered`, `automation.rs:455`).

Wet/dry is the `HostWetDry` law from `insert_processing_contracts`; bypass
is `ExplicitBypass` (pass-through, no latency). Latency: the three effects
above are zero-latency; `latency_samples` stays 0 and `bus_insert_latency`
keeps working when a later effect declares some.

## Tileability, honestly

Each effect declares its own history bound in `NativeNode::timing`
(`compiled_audio_graph.rs:227`), the way `add_instrument` derives
`lookbehind_frames` from the voice's release (`:469, :2059-2110`):

- Filter (SVF): lookbehind = frames for the slowest reachable pole to decay
  `effects::HISTORY_MERGE_BITS` (512 bits), clamped to the plan extent. The
  criterion is a *measured* merge tail, not a decay threshold: two `f32`
  trajectories re-inject an ulp every time they round differently, so the
  merge is a race whose length is a random variable (median 36 bits, p99.99
  141, worst of three million boundaries 232). 512 is thirty e-folds past
  that worst case; the measurement is in `effects`'s module doc.
- EQ: no bound. Its direct-form-I peaking section was measured never to reach
  the whole render's bits, at 0 dB or +18 dB, at any preroll. An EQ insert
  therefore declares an unbounded history and every project longer than a
  tile context renders whole bounces by name. Giving the EQ the filter's
  trapezoidal topology would make it tileable and would change what it
  sounds like; that is a deliberate decision, not a fix.
- Compressor: lookbehind from the slower of attack/release under the same
  criterion, which exceeds a tile context at every release it can reach.

A tile's context *is* its preroll: `TileLayout` extends it back by the
declared bound and `render_tile` renders it with
`HistorySupply::Span`, so the engine's own seek does not preroll it a second
time (`compiled_audio_graph::HistorySupply`).

Effect state below `effects::STATE_FLOOR` (2^-40) is exactly zero, in the
kernels, on both paths: without it a tile whose preroll lies inside digital
silence renders exact zeros against the whole bounce's decaying tail and
every core sample differs.

The controller's two-pass probe (`project_audio_controller.rs:479-495`)
plans under `declared.covering(native)`, so no plan-side edit is needed. The
cost is real and must be stated: `canonical_boundary_recipe`
(`render_tiles.rs:136`) hashes the bound, so changing a filter's cutoff
range or a compressor's release re-renders every tile; parameter changes
within the bound invalidate the bus only (`derive_change_set`,
`command.rs:1386`, already invalidates every bus on any mixer change; a
finer rule is a separate subtraction).

## What the strip does

`+ insert` opens a picker of the three native effects (replacing
`RequestInsert`'s permanent refusal at `control_actions.rs:908`); the insert
row shows the effect's name and its parameters as draggable controls (the
`MixerControl::InsertWet` pattern), "bypassed" / "active" without "not
rendered"; hosted (`clap`) descriptors that persisted from older projects
keep the "not rendered · plugin hosting is offline-only in a later build"
label, so the truth stays visible per row.

## Deletions

None of the mixer model. The three strings and `RequestInsert`'s refusal go
because they become false.

## Gate

Headless: a compiled graph with a bus insert renders differently from
without (engine_regression: low-pass at 200 Hz on the master drops the
spectral centroid of a broadband clip by an asserted ratio; compressor
reduces crest factor); tileability: the same project renders byte-identically
whole and tiled (`render_tiles` equivalence test) with the insert present.
Live: `+ insert` by pane, or a new `audec.mixer.insert_filter` action for
the socket; export master with and without the insert; sox `stat` shows the
change; `status` shows the strip label "active".
