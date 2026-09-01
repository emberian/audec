# Coverage and the Explains unification: making the equation visible

Status: implementation contract, updated 2026-09-01.
Normative core for the COVERAGE workstream in [SWARM_PLAN.md](SWARM_PLAN.md);
the final section is explicitly speculative. Related contracts:
[COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md) (ChangeSet),
[RENDER_TILES.md](RENDER_TILES.md) (tile discipline),
[LANGUAGES.md](LANGUAGES.md) (aspects), [READINGS.md](READINGS.md)
(portability of comparisons).

## Implemented product map

The UI-neutral surface now lives in `coverage.rs` and is fed by the existing
comparison runtime rather than by a lens-local renderer:

- `compute_coverage_span` is the one span-addressable form of the existing
  explained/residual/excess equation. Coverage tiling calls it directly; a
  tile is an execution partition, not another STFT implementation.
- `CoverageTilePlanner::plan_viewport` maps a viewport onto stable
  power-of-two project-frame tiles and resolves the FFT/hop from physical
  column width. Zooming therefore produces a new numeric key, while panning at
  one scale can retain grid-aligned tiles.
- `CoverageProductInputs` accepts the immutable source, construction, and
  residual `RenderProduct`s emitted by `ComparisonRuntime`. It verifies exact
  alignment and verifies `residual == source - construction` over every FFT
  support span before analysis. No coverage path compiles an explanation,
  renders audio, fits gain, or subtracts again.
- A tile key retains the durable `ComparisonId`/`ExplanationId` and hashes the
  exact PCM slices read by its windows. Consequently a whole render product
  may change elsewhere without invalidating an unaffected tile. `ChangeSet`
  ranges guide scheduling and diagnostics; content identity is the final reuse
  proof, including the honest case where a reported dirty range rendered to
  identical samples.
- `CoverageWorkbenchPresenter` owns only cache and layer selection. A cell
  click returns a `ConcreteAspect`, a persistent-comparison reveal target, and
  shared-product audition pins. Explained and residual cells have direct PCM;
  excess never fabricates PCM and always carries a residual companion
  audition.
- `CoverageAccountingDiagnostics` publishes the phase cross-term and overlap
  counts. Explained, residual, and excess are explicitly non-additive and must
  never be rendered as slices of a 100% stack. Coverage remains energy
  accounting, never correctness or confidence.

audec's project-level type signature is
`source = Σ(explanations) + residual`. Today each lens proves its own piece
of that equation privately: Loom owns `FitMetrics`, HPSS owns
`HpssDiagnostics`, the render path owns `GoldenFingerprint`. This document
unifies them: **one compiled-explanation contract, one explained-energy
field, one persistent comparison object.** Coverage is a signal-energy
measurement, not a perceptual claim, and a high coverage number is never
evidence that an explanation is *correct* — only that it is *sufficient to
reconstruct*. The UI must not present coverage as quality.

## 1. The Explains contract

The tree's uniform idiom is compile-then-render (immutable inputs frozen on
the control thread, pure rendering after), not traits scattered on mutable
domain data. The unification therefore is a **compiler to a common product**,
not a trait implemented by `Clip`, `SequenceSketch`, and friends:

```rust
/// What is being asked to explain. Scopes are project references, not
/// audio: compiling resolves them against one frozen snapshot.
#[derive(Clone, Debug, PartialEq)]
pub enum ExplanationScope {
    /// One arrangement clip, rendered through its track/bus path solo.
    Clip(arrangement::ClipId),
    /// One pattern placement with its routed instruments.
    PatternClip(sequencer::PatternClipId),
    /// One reconstruction track from a (possibly unapplied) proposal.
    ReconstructionTrack {
        proposal: reconstruction::ReconstructionProposalId,
        track: reconstruction::ReconstructionTrackId,
    },
    /// Selected Loom clusters over their analysis span.
    LoomSketch {
        analysis: aspect::AnalysisRef,
        clusters: Vec<usize>,
    },
    /// One HPSS component of a selection-local separation.
    HpssComponent {
        span: aspect::FrameSpan,
        component: HpssComponentKind, // Harmonic | Percussive
    },
    /// A model-worker output claim (never presumed a stem).
    ModelClaim(ModelClaimRef),
    /// Any union of the above; renders sum, evidence concatenates.
    Group(Vec<ExplanationScope>),
}

/// Frozen, renderable explanation. Construction validates the scope against
/// the snapshot; rendering after construction cannot observe later edits.
pub struct CompiledExplanation { /* frozen schedule/synth/mask inputs */ }

impl CompiledExplanation {
    pub fn scope(&self) -> &ExplanationScope;
    /// Where this explanation claims to explain, as an aspect. Subtraction
    /// and coverage are evaluated only inside this extent.
    pub fn extent(&self) -> &aspect::ConcreteAspect;
    /// Render this explanation alone for an exact half-open window, in
    /// project frames, at project rate/channels.
    pub fn render(
        &self,
        window: aspect::FrameSpan,
        cancellation: &daw_render::RenderCancellation,
    ) -> Result<audio::ProjectAudio, ExplainError>;
    /// Typed evidence references (ontology and/or reconstruction IDs).
    pub fn evidence(&self) -> &[ExplanationEvidenceRef];
}

pub fn compile_explanation(
    scope: &ExplanationScope,
    snapshot: &live_project::LiveProjectSnapshot,
    config: &daw_engine::DawEngineConfig,
) -> Result<CompiledExplanation, ExplainError>;
```

Backing implementations reuse what exists: `Clip`/`PatternClip` scopes
compile a solo-restricted `DawEngineSchedule` and call its windowed
`render`; `LoomSketch` wraps `SequenceSketch::render_span`; `HpssComponent`
wraps `ComplexStft::synthesize_masked`; `ModelClaim` reads a staged worker
artifact. Every arm produces project-frame-aligned audio, which is what
makes subtraction well-defined.

**Subtraction is exact, never gain-fitted.**
`residual(window) = source(window) - Σ renders(window)` sample-for-sample.
A least-squares gain that *would* improve the fit is reported as a metric
(`suggested_gain`), never silently applied — applying it is an edit, and
edits go through the command envelope.

**Negative claims.** A `CompiledExplanation` does not assert source
identity, does not claim its extent is complete, and does not survive
project edits: its validity is pinned to the snapshot revisions it was
compiled from, exactly like `DawEngineSchedule`.

## 2. The explained-energy field

The coverage view answers "where does my explanation fail?" as a
time×frequency field, computed with the same resolution honesty as
`spectral_tiles`:

- **Definition.** For an STFT recipe applied identically to the source
  window and to `Σ renders`, per cell:
  `explained = 1 - |R|² / max(|S|², floor)` clamped to `[0, 1]`, where `R`
  is the residual spectrum and `S` the source spectrum. A second channel,
  `excess = max(0, |C|² - |S|²) / max(|S|², floor)` for the construction
  spectrum `C`, makes **over-explanation visible** (constructions inventing
  energy the source does not have — clamping alone would hide it).
- **Resolution follows the view.** Coverage is served as tiles with the
  `spectral_tiles` cache-key discipline: exact frame range, recipe, pixel
  LOD. Zooming recomputes; it never enlarges a stale raster.
- **Cache identity.** A coverage tile's key includes: the source stamp, the
  spectral recipe, the tile range/LOD, and a **construction fingerprint** —
  `render_validation::GoldenFingerprint` of the summed explanation render
  over the tile's (preroll-padded) window. Fingerprints are cheap, already
  exist, and make "did my edit change this tile?" a hash compare.
- **Invalidation.** [COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md)'s `ChangeSet`
  bounds which coverage tiles can be dirty, exactly as it bounds audio tiles
  in [RENDER_TILES.md](RENDER_TILES.md). Coverage rendering rides the same
  background scheduler at strictly lower priority than audible tiles.
- **Summary scalars.** Whole-selection coverage (energy-weighted mean of
  `explained`) is displayed with its band breakdown, and always alongside
  the residual audition button. The number is never shown where the residual
  cannot be heard: audibility is the honest unit; the scalar is navigation.

## 3. Persistent comparison objects

Today original/reconstruction/residual live in lens-local buffers; closing
the lens discards the experiment (UX_WORKFLOW_AUDIT friction #7). A
comparison becomes a small persistent project object:

```rust
pub struct Comparison {
    pub id: ComparisonId,                      // typed, serialized allocator
    pub label: String,
    /// What is being explained: an exact span of one source asset.
    pub source_asset: assets::AssetId,
    pub source_range: reconstruction::SourceFrameRange,
    /// What explains it.
    pub scope: ExplanationScope,
    /// The revisions this comparison was last rendered against.
    pub revisions: daw_project::ProjectRevisions,
    /// Render identity at those revisions.
    pub construction_fingerprint: render_validation::GoldenFingerprint,
    pub residual_fingerprint: render_validation::GoldenFingerprint,
    /// Signal metrics at those revisions (explained energy, residual RMS,
    /// correlation, per-band summary, suggested_gain).
    pub metrics: ComparisonMetrics,
    pub provenance: ontology::Provenance,
}
```

Semantics:

1. **Creating, re-rendering, and deleting a comparison are envelope
   commands** — undoable, journaled, and eligible to travel in a reading.
2. **Staleness is visible, never silent.** When the live aggregate's
   generations diverge from `comparison.revisions`, the comparison shows a
   stale badge; its stored metrics/fingerprints remain those of the recorded
   revisions. Refreshing is an explicit command that re-renders and replaces
   fingerprints + metrics + revisions atomically.
3. **The A/B strip binds to a `ComparisonId`.** Original | Construction |
   Residual switching is gain-matched routing state on the audition bus, not
   a project edit; the strip works identically for every `ExplanationScope`
   because every scope compiles to the same product.
4. **Persistence** is a new domain section in the `project_io` envelope
   (codec-era DTO; unknown-field preservation as everywhere). Comparisons
   referencing scopes that no longer resolve load as visible placeholders
   with their stored metrics intact — a dead comparison is still a record of
   an experiment.

## 4. Acceptance contract

- Compiling and rendering any scope twice from the same snapshot is
  byte-identical; a Loom scope's render equals `SequenceSketch::render_span`
  for the same inputs (no second implementation drift).
- `source == construction + residual` holds exactly (float-exact
  subtraction) for every scope over its extent.
- Coverage tiles: recompute-on-zoom, never bitmap enlargement; a project
  edit outside a tile's ChangeSet regions provably reuses the tile
  (fingerprint unchanged); the `excess` channel lights up on a deliberately
  over-gained construction fixture.
- Comparison round-trip: save/reopen preserves metrics, fingerprints,
  revisions, and provenance byte-for-byte; stale detection fires after an
  edit; refresh-by-command updates exactly the stored fields and is
  undoable.
- No UI surface presents coverage as correctness; the residual is auditionable
  from every place a coverage number appears.

## 5. Speculative (explicitly non-normative)

- **Coverage-guided suggestion.** Rank "what should I try next?" by which
  lens family historically raised `explained` for cells with similar
  feature signatures (band, flatness, transient density). Turns coverage
  from a scoreboard into a navigator; requires no new inference, only
  bookkeeping over past comparisons.
- **Union coverage as a collaboration metric.** For two readings of one
  source: coverage(A), coverage(B), coverage(A ∪ B) — the union's excess
  over each alone measures how *complementary* two hearings are, which is a
  better collaboration prompt than agreement.
- **Coverage diffs over time.** A reading's coverage history (per
  comparison refresh) is a progress curve; plotting it makes decompilation
  feel like the game loop it secretly is.
- **Excess as a hallucination detector for ML claims.** Model-claim scopes
  with high `excess` are inventing energy; surfacing that per-claim is a
  cheap, honest model-quality signal no separation SDR metric provides.
