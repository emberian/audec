# Incremental render tiles: one engine, edit-while-looping

Status: normative interface design, 2026-08-31, against `main` at `dbdda7b`.
Ground truth for the TILES workstream in [SWARM_PLAN.md](SWARM_PLAN.md).

## Doctrine

Bounce-on-play is the engine, not a placeholder: playback is the offline
render, so online/offline nulling holds by construction. This design makes
the bounce *incremental* instead of introducing a second, realtime engine.
The target experience: loop a bar, mute an event, hear the change on the
next loop pass without stopping transport.

Non-goals, permanently for this workstream: no lock-free graph, no plugin
callbacks, no live input monitoring (deferred with recording, roadmap
gate 10), no divergence between what Play renders and what Export writes.

## What already exists (and is kept)

- `daw_engine::compile_daw_engine(project, pcm, window, config, cancellation)
  -> DawEngineSchedule` compiles an immutable schedule for an explicit
  `RenderWindow`; schedules cannot observe later edits.
- `daw_project::ProjectRevisions` carries per-domain generation counters,
  documented as existing so "caches invalidate only the products they
  consume". This design is that consumer.
- `audio::ProjectRenderer` is the seam the transport source pulls from; the
  tile player implements it without touching `TransportSource`.
- [COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md)'s `ChangeSet` provides typed
  dirty regions per envelope. Coarse coverage is legal; wrong is not.

## Tile identity

```rust
/// Power-of-two tile length in project frames (default 1 << 16 at 44.1 kHz
/// ≈ 1.49 s). A tile is identified by its index on the absolute project
/// timeline: tile i covers [i * tile_frames, (i + 1) * tile_frames).
pub struct TileGrid { pub tile_frames: u32 }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    /// Master-bus mix in v1; per-bus tiles arrive with stem workflows.
    pub scope: TileScope,              // v1: TileScope::Master
    pub tile_index: i64,               // signed: preroll tiles exist
    pub tile_frames: u32,
    /// The generations of every domain this scope consumes, plus the engine
    /// config hash. Any consumed-generation change makes a fresh key; stale
    /// tiles are unreachable rather than invalidated in place.
    pub inputs: TileInputStamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TileInputStamp {
    pub consumed: ConsumedGenerations,  // subset of ProjectRevisions
    pub engine_config_hash: u64,        // instruments, block size, channels
}
```

**Dirty mapping.** v1 correctness first: `TileScope::Master` consumes every
domain generation, so any committed envelope refreshes all not-yet-rendered
tiles (cheap: keys simply change). Precision second: when the ENVELOPE
`ChangeSet` is available, unchanged-audio proof narrows re-rendering to
tiles intersecting `change_set.audio` ranges — implemented as a *reuse*
rule (a tile whose audio provably did not change may be re-keyed to the new
stamp without re-rendering) so precision never risks correctness.

## Tail-correct tile rendering

A tile cannot be rendered from its own start frame alone: a sampler voice or
clip fade triggered before the tile may still be sounding. Every tile render
therefore uses an explicit preroll:

```rust
pub struct TailPolicy {
    /// Preroll rendered before the tile and discarded. Must cover the
    /// longest audible tail that can cross a tile boundary (longest sample
    /// remaining after its trigger + release, longest fade, longest send
    /// tail in the built-in path).
    pub preroll_frames: u64,
}
```

The compiler computes the required preroll from the schedule itself (max
event tail crossing into the window); a configured ceiling bounds it, and an
event whose tail exceeds the ceiling emits a typed diagnostic naming the
event — never a silent truncation.

**The null law (the test that keeps one engine):** for any revision and any
tile grid, concatenating rendered tiles equals the single whole-window
render **byte-exactly**. This is a mandatory regression test with
adversarial fixtures: voices sustained across boundaries, fades spanning
tiles, loop-boundary ratchets, sends with tails.

## Scheduler and playback

```rust
/// Owned by the control thread. Renders missing tiles ahead of the playhead
/// (and around the active loop) via background workers using the existing
/// compile + cancellation seams.
pub struct TileScheduler { /* queue, workers, RenderCancellation per job */ }

/// Priority: tiles inside the active loop first (nearest-ahead-of-playhead
/// first), then lookahead beyond it. An envelope commit cancels queued jobs
/// whose keys became unreachable.
```

Playback swaps tables, not samples:

```rust
/// Implements audio::ProjectRenderer. The realtime pull path reads one
/// atomic Arc<TileTable> snapshot per rendered block: no locks, no
/// allocation, no waiting.
pub struct TiledRenderer { /* Arc-swapped TileTable, position, diagnostics */ }
```

- A requested frame whose tile is absent renders **silence and increments a
  visible starvation counter** — never a blocking wait in the pull path.
- Tables swap between blocks; a swap mid-loop takes effect exactly at the
  next block boundary, which is what makes "hear the edit on the next pass"
  true without transport interruption.
- The existing whole-project bounce path remains as the Export path and as
  the oracle in the null-law tests.

## Cache policy

- Tiles are immutable once published; the store is keyed by `TileKey` and
  bounded by a byte budget with LRU eviction.
- Pinned set: tiles covering the active loop plus the lookahead horizon are
  never evicted while transport rolls.
- Eviction and starvation are observable (counters + a status surface),
  because an inaudible cache failure is an epistemic bug, not just a
  performance one.

## Acceptance contract

1. Null law: tile concatenation equals whole-window render byte-exactly on
   the adversarial fixture corpus.
2. Edit inside an active loop is audible on the following loop pass with
   transport uninterrupted; edit outside the loop causes zero re-renders of
   in-loop tiles once ChangeSet reuse lands (cache-hit assertion).
3. Starvation renders silence with an incremented counter, never a glitch,
   block, or stale-revision tile.
4. Cancellation: a superseded tile job stops promptly and publishes nothing.
5. Export equals playback: the exported WAV for a revision equals the
   concatenated tile stream for that revision byte-exactly.
