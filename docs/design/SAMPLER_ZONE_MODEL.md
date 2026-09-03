# Sampler zone model (cycle 1, lane C1-Sampler)

Resolves the sampler rows of `UX_EXPOSURE_AUDIT.md`: LOOP RANGE, PING PONG,
and the envelope readout acknowledge in green and persist nothing; TRIM's
error is overwritten; `emit` drops actions silently; REVEAL ↗ and KIT ‹ ›
do nothing.

## Today

`SampleZone` (`src/sample_kit.rs`) has `material`, `gain_db`, `pan`,
`tuning_cents`, `decoded_pcm`, `provenance`, `evidence`. The intent
vocabulary already exists: `ZoneEditIntent::{Trim, SetLoop, SetEnvelope,
SetPlayback}` with `SampleLoopMode::{Forward, PingPong}` and
`SampleEnvelopeIntent{attack_frames, decay_frames, sustain, release_frames}`
(`src/sample_actions.rs:289-300`). The controller validates `SetLoop` and
`SetEnvelope` and then returns `SampleActionOutcome::ForwardZoneEdit(intent)`
(`src/constructive_controller.rs:484-543`), which the pane reports as "Edit
retained for its owning surface" (`src/pane_audio.rs:723`). The voice
(`src/instruments.rs:747 SampleVoice`) plays `position += rate` from the
zone's range with gain and pan; no loop, no envelope.

## Target

```rust
pub struct SampleLoop {
    pub range: AssetFrameRange,      // within the zone's material range
    pub mode: SampleLoopMode,         // Forward | PingPong
}

pub struct SampleEnvelope {
    pub attack_frames: u64,
    pub decay_frames: u64,
    pub sustain: f32,                 // 0..=1
    pub release_frames: u64,
}

pub struct SampleZone {
    // existing fields unchanged
    pub loop_region: Option<SampleLoop>,
    pub envelope: SampleEnvelope,     // Default = percussive (A64 D4800 S0 R1200 at 48k)
}
```

- **Validation** (`SampleKit::validate`): a loop range lies within the
  zone's material range and is non-empty; envelope frames are finite and
  sustain is within 0..=1.
- **Commands**: `SetLoop` and `SetEnvelope` stop being forwarded. They go
  through `edit_kit` as one `SampleKitPut` each (the same path `+ PAD` uses),
  so they are one undoable revision and they save. The publication's focus
  is the pad; `created_pads`/`created_zones` stay empty.
- **Codec**: `SampleZoneDto` (`src/project_codecs.rs:2218`) gains
  `loop_region: Option<SampleLoopDto>` and `envelope: SampleEnvelopeDto`,
  both `#[serde(default)]`, so existing packages load unchanged and
  round-trip tests cover both new fields.
- **Voice**: `SampleVoice` gains `loop_region: Option<(f64, f64, SampleLoopMode)>`
  and an `EnvelopeStage` (attack → decay → sustain → release, release
  entered on note-off / `auto_off`). Forward loop wraps `position` to the
  loop start; ping-pong negates `rate` at both ends. A zone without a loop
  plays as today; the default envelope reproduces today's percussive
  behaviour so existing render tests stay byte-stable except where a loop
  or non-default envelope is set.
- **Pane honesty**: TRIM keeps its error status (the success line is only
  written on `Ok`); `emit` returns whether a callback existed and the pane
  shows "not connected" instead of "request sent" when it did not; REVEAL ↗
  either issues the reveal through the existing `PendingSampleFocus` path
  or is removed; KIT ‹ › / CHOOSE EXISTING KIT retarget through
  `SamplerWorkspaceIntent` handled by the host (`resolve_sample_pane_outcome`)
  or are removed. Removal is acceptable; a dead control is not.

## Gate

Flow test in `src/cycle11_flow.rs`: make a beat, set a loop on one zone,
export the master, and assert the region after the slice's natural end is
non-silent within the pattern step; undo removes the loop and restores the
previous master byte-for-byte. Live:
`scripts/live/make_beat_audible.sh` extended with a socket `sample` verb
(`{"op":"sample","edit":"loop","zone":..}`) if one is added, else the flow
test plus the existing scenario.

## Files owned

`src/sample_kit.rs`, zone arms of `src/constructive_controller.rs`,
`src/sampler_view.rs`, `src/sampler_pane.rs`, `src/pane_audio.rs` (zone
outcome mapping only), the zone DTOs in `src/project_codecs.rs`,
`src/instruments.rs` (`SampleVoice` only), `src/cycle11_flow.rs` (new test).
