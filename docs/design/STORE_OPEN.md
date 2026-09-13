# Opening the render-product store (cycle 5)

Written 2026-09-13 against `fbb5961`, from the measurement in `docs/STATE.md`'s
cycle-5 follow-ups: **the app's start-up cost was the size of its render-product
store.** Release build, *Like a Pen*: a fresh store bound the control socket in
0.29 s and reached ready in 2.6 s; the scenario store after twenty scenarios
(541,153 files, 3.3 GB) took 106 s to the socket and 225 s to ready, and a
relaunch on it had not bound its socket after 200 s. The musician's own store
held 59,488 files at the same moment, so the shipped app paid tens of seconds
per launch and more every day.

## What the cost actually was

`TileProductCache::open(store, owner)` (src/render_tiles.rs:436) did all of
this before returning, on the main thread, inside `create_workspace` — which is
inside `cx.open_window`, which runs before `install_control_socket` and before
the first frame:

1. `store.inventory()` — `regular_files_below(objects/)`, then one
   `inspect_path` per object: `symlink_metadata`, `open`, read and decode the
   header, check the length against it, and check the path is the canonical one
   for the digest. Two syscalls and a decode per object, for every object the
   store has ever held.
2. For every object whose schema is the receipt schema,
   `RenderProductCatalog::read_receipt` — a full `read_verified` of the
   manifest (read the bytes, SHA-256 them, compare) plus a JSON parse and a
   `ProductKey::decode_canonical`.
3. `cache.adopt(receipt)` — `pin_object(manifest)` and `pin_object(payload)`.
   Each pin takes the store-wide GC gate (a `create_dir` of `pins/.gc-gate`),
   calls `verify(object)`, **which reads and SHA-256s the whole payload**,
   writes a pin file, `fsync`s it, and `fsync`s the owner directory. Two of
   these per receipt, serialized on one gate.

So a launch was one directory walk plus, per receipt, a manifest read, a
whole-payload hash, and two fsynced pin writes under a global lock. Every
adopted receipt also stayed in `entries: BTreeMap<ProductKey,
RenderProductReceipt>` for the life of the process (554 MB RSS on that store
against 267 MB on a fresh one). Cycle 4 put the resident *products* under a
budget (`PinnedLru`, 256 MiB); the *receipts* were never under one, because
adoption was treated as free.

None of that work is needed to start. A launch does not know which tiles it
will want, and most launches want none of them: the tiles a render asks for are
the ones covering the loop it is about to play.

## The decision

**A receipt is read, pinned and adopted by the render that asks for its
recipe.** Nothing is read at open.

For that to be possible the store needs to answer "the receipt for *this*
request key" without a walk. The receipt's own address cannot answer it: a
receipt object is addressed by the SHA-256 of its own bytes, and those bytes
contain the payload digest, which is a fact about the render that only exists
*after* the render. The request key is known before. So the map from one to the
other has to be written down.

**It is written down as one small file per request, named by the request.**
`FsContentStore` grows a reference namespace: `refs/<namespace>/<ab>/<hex>.ref`
holds the canonical encoding of one `ObjectRef`. `TileProductCache` uses the
namespace `render-request-v1` with the name
`request.digest().sha256().to_hex()`. A hydrate is then:

    open(refs/render-request-v1/ab/abcd….ref) -> manifest ObjectRef
    read_verified(manifest) -> receipt          (digest-checked by the CAS)
    receipt.request == request?                 (the hint is checked, not believed)
    adopt(receipt)                              (two pins, for this one tile)

— a constant number of reads, whatever the store holds.

### Why not one appended index file

`render-products/index.v1` as a single appended log is the obvious shape and it
is worse here on every axis that matters. Reading it at open is O(entries)
again — cheaper per entry than the walk, but still ~50 MB and half a million
`ProductKey` decodes on the store we measured, and it puts every entry back in
memory, which is the other half of the problem. Appending from several
processes needs a lock the CAS does not otherwise need (the app already runs
beside scripted instances on one cache root). Rewriting it to drop dead entries
needs a compaction pass. A directory of name-addressed files needs none of
that: `create_new` is the whole concurrency story, a dead entry is removed by
the reader that found it dead, and the only thing in memory is what this session
actually touched.

The directory is sharded by the first two hex characters of the name, matching
the `objects/` layout, so no directory holds more than a few thousand entries at
the store sizes we have measured.

### Why the store owns it and not the tile cache

The reference is a name pointing at immutable bytes, and every rule that makes
the CAS trustworthy applies to it: never follow a symlink, create atomically
with `create_new`, `fsync` the file and its directory, never overwrite. Those
rules live in `content_store.rs` and should not be reimplemented next door. The
store still infers nothing: **a reference is a hint, never a root.** GC does not
read `refs/`, a reference does not keep an object alive, and a reference whose
object is gone is a cache miss with a name.

## Consequences, stated plainly

- **Pins at start: none.** This is the decision the brief asks for. Pinning at
  launch was never about the launch; it was a side effect of adopting. A tile
  is pinned when a render hydrates it or publishes it, and those pins are
  released when the process exits cleanly. Nothing is pinned on behalf of a
  cohort that no longer exists, which also means a killed instance now strands
  two pin files per *used* tile instead of two per *stored* receipt.
- **`entry_count()` means "receipts adopted this session".** `status.memory`'s
  `tile_cache_receipts` therefore reads 0 on a fresh launch of a huge store and
  rises as tiles are hit. That is the number we want: the old one reported the
  size of the disk, which no budget bounded.
- **Cross-session ambiguity is detected by the index pass, not by every open.**
  Two receipts naming disagreeing PCM for one request key mean the engine was
  not deterministic; the old open found that by comparing every receipt it
  walked. Now a second, disagreeing publication is caught at publish time (the
  reference already exists and names another manifest), and a pre-existing
  disagreement is caught by the one-time index pass. A store that was never
  walked can serve the first of two disagreeing receipts. The payload is still
  digest-verified and its `produced_by` still has to match the spec, so what is
  served is always *a* render of exactly these inputs — but we are no longer
  guaranteed to notice that there were two.
- **The pin cost moves from every launch to the renders that use tiles, and it
  is measurable.** The old build pinned every receipt at open, so a later
  publish of the same payload found the pin already held and wrote only the
  receipt. The new build pins two objects per published or hydrated tile
  instead, and writes a reference. Measured on the same material, same store:
  the first export of a 6:13 master went 16.7 s → 18.4 s, and a second launch's
  export 8.8 s → 12.9 s (252 tiles; roughly 16 ms per tile of gated,
  fsynced pin and reference writing). That is the trade this note is making:
  seconds spread over the renders that want tiles, instead of minutes on every
  launch whether it wants any or not. The obvious reductions, in order —
  pin a receipt's two objects under one GC-gate acquisition instead of two, and
  skip the pin's whole-payload re-verification for bytes this process just
  wrote and hashed — are left for a lane that owns the pin API.
- **A crash between publishing the object and writing the reference orphans a
  receipt.** The order is object-then-reference on purpose: a dangling
  reference costs one failed read and is removed on the spot, while an
  unreferenced object costs nothing until GC collects it. The cost of the
  orphan is one re-render.

## The walk that is left

Two things still want the whole store, and neither is a launch:

1. **The one-time index pass.** A store written before this change has no
   references, so every hydrate would miss and every tile would be re-rendered.
   `rebuild_render_request_index(&store)` walks the objects, reads each tile
   receipt, and writes the missing references. It runs on the background
   executor a couple of seconds after the window exists, on a *clone of the
   store handle, without the cache lock*, so a render that wants a tile in the
   meantime is never blocked by it (it misses and renders, which is what it
   would have done anyway). When it finishes it marks the namespace
   (`refs/render-request-v1/.mark-complete`) and no later launch walks again.
   Its diagnostics — including the `cas-inventory` ones the old open reported —
   are deposited into the cache, capped, and drained by the render path exactly
   as before.
2. **GC.** `plan_gc` already walks, and should: collection is the operation
   that is allowed to be slow and is never on the launch path.

## The socket is served first

`install_control_socket` now binds the listener *before* `cx.open_window`, and
installs the main-thread poller after the window exists. Binding is
`UnixListener::bind` plus an accept thread; a request that arrives before the
window is up waits in the mailbox and is answered when the poller starts (well
inside `REPLY_TIMEOUT`). The point is that "is the app alive" stops being a
question about the store: the socket file exists as soon as the process is
running, whatever `create_workspace` is doing.

`status.store` reports what the store is doing without blocking on any walk:
`state` (`opening` while the index pass runs, else `ready`), `adopted` (receipts
this session pinned), and an `index` object naming the pass's state, objects
seen, receipts indexed, references written, elapsed milliseconds, and its
failure if it had one.

## What it measured

Debug build (this repo's `dev` profile is `opt-level` + debuginfo), *Like a
Pen*, a store of 100,000 synthetic-but-real tile receipts (300,001 files,
1.9 GB, written by `render_tiles::tests::fill_a_store_for_measurement`), one
laptop shared with other work. The absolute numbers are debug numbers; the
ratio between the two columns is the claim.

| launch | → socket | → ready | RSS |
| --- | --- | --- | --- |
| after · fresh store | 1.15 s | 3.39 s | 332 MB |
| after · 100,000 receipts | 0.12 s | 2.36 s | 334 MB |
| after · 100,000 receipts, index not yet built | 0.11 s | 1.29 s | 328 MB |
| before · fresh store | 0.23 s | 2.43 s | 331 MB |
| before · 100,000 receipts | **never bound within 900 s** | — | 501 MB and climbing at 11 min |

The baseline, killed at the 900 s bound, had left **124,650 pin files** behind
(the store went from 303,278 to 427,928 files): it was roughly two thirds of
the way through adopting a store it had not yet been asked for one tile of.

The index pass on the same store — every receipt read and checked — took
93.4 s for 100,571 receipts, on the background executor, while the app had
been up for 1.3 s and was exporting. `scripts/live/store_open.sh` is the
scenario.

## What the measurement found next

A tile published by one launch is **not** reused by the next, and never was.
Two launches on one fresh store, same material, no edits, leave 504 receipts
and 252 payload objects: the PCM is bit-identical and deduplicated by content,
while the *request keys* are all different. Decoding those receipts, the two
sets differ in exactly one field — `plan.snapshot`, the project-audio snapshot
digest — with the project namespace, extent and engine configuration
identical. `project_audio_snapshot_digest` hashes the whole constructive
encoding, and `assets.json` in that encoding carries `imported_at_unix_ms`,
the wall clock at import. So importing the same file twice is, to the cache, a
different project.

The baseline build does the same thing — two launches under `audec-before`
leave the same 504 receipts over 252 payloads — so this is not something the
on-demand adoption introduced; it is what the store was always doing.

That is the reason these stores reach half a million files: every launch
renders every tile again and keeps a second, third, fourth copy of the receipt
forever, and nothing ever reads one back. It is upstream of this note (nothing
here changes a key) and is left for a lane that owns the project encoding; the
honest summary until then is that the render-product store is a within-session
cache with permanent, unbounded storage. Fixing it is what makes the on-demand
adoption above actually pay.

## What this does not change

Byte identity. Nothing about what a tile *is* moved: `tile_product_request`,
the receipt format, `persisted_derivation_matches`, and the rehydrate path are
untouched. A cache hit produces the same PCM it did before, and
`make_beat_audible.sh`'s exports are byte-identical across the change.
