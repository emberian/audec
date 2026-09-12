# Streaming material (cycle 4)

Written 2026-09-12 from a verified map of the tree at `98e372d` (the
facts, with file:line, are in the cycle-4 briefings; this note states the
design). Measured on the release build: a 4:47 stereo 48 kHz mp3 costs
813 MB of resident memory after open and 54 s to "ready"; a 6:13 flac
reaches ready in about 2 s. audec will be pointed at hours-long material,
so both numbers have to become functions of the window a musician is
looking at, not of the file.

## What the tree already has

A complete, tested, seeking, budgeted chunk layer with no production
caller: `ProjectRateChunkSource::PacketStream` (a real
`format.seek(Accurate, TimeStamp)` per chunk), `StreamingGraphMediaSource`
(a `MediaBlockProvider` factory over lease snapshots), `BoundedMediaStore`
(two-tier LRU with pin counts and `CacheBudgets`), viewport and prefetch
planners, and a `StreamingWaveformIndex` that survives PCM eviction. The
disk tier is accounting only (no filesystem adapter). Every consumer
below it still asks for one contiguous `&[f32]`.

The canonical PCM format (`CANONICAL_PCM_MAGIC`, header plus little-endian
f32 bits) is exactly a memory-mappable image of decoded material, and
`encode_canonical_pcm` already writes it. Nothing persists a decoded copy
today: every open re-decodes the whole file into one `Vec<f32>` (with a
2× reallocation transient and a second copy into `Arc<[f32]>`), then the
pyramid copies it again, then the session copies it again. Three
whole-file reads of the compressed bytes happen per open.

## The design, in the order that pays

1. **One decoded image, mapped, content-addressed.** Decode writes into
   the canonical-PCM file under the tile CAS (keyed by the source
   fingerprint and the project rate); the reader memory-maps it. `PcmAsset`,
   `ProjectAudio` and the waveform pyramid are backed by the mapping; their
   `&[f32]` accessors keep their signatures. `Analysis::mono_pcm` becomes a
   range read over the stereo map (`mono_range` is already range-based).
   The fingerprint is taken from the map, not from a third read. A second
   open of the same material is a cache hit: no decode at all. This moves
   ~460 MB of the measured 813 MB from anonymous memory to evictable page
   cache without changing a consumer signature, and it is the base the
   rest stands on.
2. **Lenses ask for windows.** HPSS already bounds itself to 30 s. Rhythm
   drops its whole-file copy and computes novelty streaming over chunks
   with FFT-sized overlap (novelty is causal). Loom bounds template
   extraction to a lookbehind around the selection. The waterfall and the
   spectrogram detail go through `compute_spectral_tile_streamed` with a
   `SpectralTileCache` on the Workbench (the streamed form exists; the UI
   calls the whole-slice form). Components are already
   duration-independent. Every window-bounded lens says which window in
   its header.
3. **Playback before completion.** `CohortRenderer` accepts a `Priming`
   cohort and falls back per slot to the previous cohort
   (`PlaybackCohort::covers` is the predicate); the previous cohort keeps
   tile receipts and rehydrates from the tile CAS instead of pinning a
   second master; `RenderProductCatalog` sits under `BoundedMediaStore`'s
   pin/LRU; instruments declare `BoundedHistory` so tiling stops bailing
   to whole bounces. Export walks the cohort's products in slot order and
   encodes per tile; the adaptive tail is the only cross-tile read.
4. **Progressive open.** The packet-stream chunk source becomes the
   production decoder, feeding the mapped image incrementally with a
   filesystem adapter for the disk tier; ready means the first screen is
   true (pyramid and overview from the chunks decoded so far), analyses
   arrive as freshness authorities, and `status.memory` reports the
   resident budget, resident bytes and disk bytes so a scenario can read
   them.

## Invariants

- One PCM truth per material; every reader goes through the same windowed
  interface; no reader holds a whole-file buffer of its own.
- Bounded resident memory with the number visible in `status`.
- Tiles are the unit for analysis and render alike; each window-bounded
  product declares its history the way inserts declare theirs.
- Nothing lies about what it has: a lens over a window names the window;
  a whole bounce names its reason; a partial decode names its extent.

## Waves

Wave 1 (disjoint files): C4-Cache (item 1), C4-Lenses (item 2),
C4-Render (item 3). Wave 2: C4-Stream (item 4), which builds on the
mapped image. Gates are measured in resident bytes and seconds to ready
on the same mp3, and in byte identity of exports and lens products where
semantics did not change.
