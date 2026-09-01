# Swarm plan: the reverse/forward campaign

Status: planning synthesis on 2026-08-31 against `main` at `dbdda7b` (385 tests
passing, 1 ignored). This document operationalizes a full read of the tree and
docs into swarm-cyclable workstreams. It is a delivery instrument, not a vision
document: [VISION.md](VISION.md) owns the epistemology,
[DAW_ARCHITECTURE.md](DAW_ARCHITECTURE.md) the engine contract,
[DAW_UI_BLUEPRINT.md](DAW_UI_BLUEPRINT.md) the interaction contract,
[LMMS_PARITY_ROADMAP.md](LMMS_PARITY_ROADMAP.md) the acceptance gates,
[UX_WORKFLOW_AUDIT.md](UX_WORKFLOW_AUDIT.md) the ranked friction list, and
[ML_MODELS.md](ML_MODELS.md) the model procurement locks. This document owns
**what runs concurrently, in what order, with what briefing and what gate**.

> **Execution update:** after the interface groundwork landed at `50d61ce`, a
> second architecture swarm read the live command, GPUI, render, persistence,
> constructive, worker, and interpretation boundaries. The current broad
> brick/convergence schedule is [SWARM_CYCLES.md](SWARM_CYCLES.md). The lane
> briefs and gates below remain the detailed capability index; their original
> cycle numbers are no longer a strict execution order.

Interface groundwork for the highest-coupling workstreams is pinned in three
sibling design documents, written so implementation lanes copy contracts
instead of reconstructing them: [COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md)
(ENVELOPE), [LANGUAGES.md](LANGUAGES.md) (ASPECT, NOTATION, NOTEWIRE,
QUERY, EXPLAIN, and the external-protocol frame),
[RENDER_TILES.md](RENDER_TILES.md) (TILES), [COVERAGE.md](COVERAGE.md)
(COVERAGE: the Explains compiler, the explained/excess field, persistent
comparisons), and [READINGS.md](READINGS.md) (READINGS: portable reading
envelopes, verification tiers, merge-as-coexisting-hypotheses).
Compiling skeletons for the new modules live in `src/{command, aspect,
pattern_lang, curve_lang, air_query}.rs`; rustc checks their coherence, and
`todo!()` bodies mark exactly what each lane owns. Where a design doc and the tree
disagree, the tree wins for existing types and the design doc wins for new
ones; a lane that must deviate records the deviation in its report and the
checker reconciles the doc afterward.

## 0. Swarm-cycle protocol

These rules are constraints on how lanes are briefed and integrated. They were
paid for in prior debugging and are not optional.

1. **Ground truth in the briefing, or the lane builds a mirror.** Every lane
   prompt pastes the *real, current* signatures and struct fields it will build
   against (copied from the tree at briefing time, not from this document or
   any other doc — docs describe intent; the tree is truth) plus absolute
   paths for every file it may touch. Never say "put it in the sequencer";
   say `/Users/ember/dev/audec/src/sequencer.rs`.
2. **Green + self-reported done is not verification.** Every cycle ends with a
   checker pass that reads the actual diffs and test *statements*, hunts for
   vacuous or tautological tests, builds the whole tree, and runs the full
   suite. The checker's verdict counts; the lane's summary does not.
3. **Whole-tree build after any shared-struct change.** Per-file green hides a
   red umbrella. Any lane that touches a struct or enum used outside its
   module triggers a full `cargo build && cargo test` before its work is
   considered landed.
4. **Test policy for this repo:** the full suite is in-memory and fast, so
   `cargo test` is the standard integration gate. During iteration, lanes run
   module filters (`cargo test sequencer::`, `cargo test pattern_lang::`).
   State the lane's *control* explicitly: for refactor lanes it is "all
   existing tests pass unchanged, no public API change beyond deletions."
5. **Concurrency discipline:** lanes commit *named files only* (never
   `git add -A`), never stash, and never touch a file assigned to another
   active lane. `src/ui.rs`, `src/live_project.rs`, and `src/daw_project.rs`
   are single-writer files: at most one active lane each per cycle.
6. **Defaults carry the project's values.** Lane prompts must state audec's
   defaults explicitly: deterministic seeds; `total_cmp` with explicit
   tie-breaks; validated configs; non-finite-PCM quarantine; typed IDs, never
   index/name identity; hypotheses never silently promoted to source
   identities; diagnostics instead of silent fallbacks; module headers state
   what the module *refuses* to claim.

## 1. Strategic frame

The campaign direction, compressed from the full-tree read:

- **Identity: a debugger for finished music.** The project's type signature is
  `source = Σ(explanations) + residual`, and the residual is the audible error
  term. Every workstream should either add an explanation kind, make the
  equation faster to re-evaluate, or make its failure more visible.
- **Doctrine 1 — bounce-on-play is the engine, not a placeholder.** Playback
  is the offline render, so online/offline nulling holds by construction. The
  upgrade path is *incremental bounce* (content-addressed render tiles keyed
  by per-domain generations), never a second realtime graph. The realtime
  callback problem class is deferred with recording (roadmap gate 10).
- **Doctrine 2 — commands over reconciliation.** One serializable command
  envelope replaces `LiveProject`'s reconcile-by-deep-diff. The envelope is
  simultaneously the aggregate undo record, the autosave journal entry, the
  gesture-coalescing unit, and the future collaboration atom.
- **Doctrine 3 — languages as data types, never an embedding.** Anything that
  changes the project passes through the command algebra; anything that
  computes is a pure, content-addressed term. Four term languages are grown in
  order: commands (verbs), aspects (nouns), pattern/curve expressions
  (generators), AIR queries (questions). Surface syntax is thin, late, and
  separate. No in-process general-purpose scripting, ever; external tools
  speak the command/query protocol.
- **Horizon — shareable readings.** A project is a claim graph about a
  recording. The long-run product is exchangeable, diffable, mergeable
  *readings*: interpretations with evidence, auditionable failure, and
  attributed perceptual language.

## 2. Workstream ledger

Parallelism classes: **pure-new** (new modules only; freely parallel),
**module-local** (bounded existing files; parallel if disjoint),
**shared-struct** (changes types used across the tree; one per cycle, whole
tree rebuilt), **solo** (cross-cutting; runs alone between cycles).

| ID | Workstream | Class | Depends on | Cycle |
| --- | --- | --- | --- | --- |
| SLICE | Sampler slice: selection → pads → steps → audible | module-local (`ui.rs` writer) | — | 1 |
| NOTATION | Pattern mini-notation core (parse/eval, no UI) | pure-new | — | 1 |
| ASPECT | Aspect algebra combinators (no syntax) | pure-new | — | 1 |
| WORKER0 | Model-worker protocol harness (fake worker) | pure-new | — | 1 |
| DSPUTIL | Deduplicate DSP primitives | module-local sweep | — | 1 |
| DIALOGS | Save/Open/Export/recovery UI | module-local (`ui.rs` writer) | — | 2 |
| ENVELOPE | Command envelope + aggregate undo | shared-struct / solo-ish | — | 2 |
| NMF1 | Unify NMF solvers (retire `decomposition.rs`) | module-local | — | 2 |
| QUERY | AIR query combinators | pure-new | — | 2 |
| NOTEWIRE | Mini-notation → live patterns + term provenance | shared-struct | SLICE, NOTATION | 2–3 |
| SPLIT | Cargo workspace split | solo | after 2 | 3 |
| TILES | Incremental render tiles; edit-while-looping | module-local (engine side) | — | 3 |
| DEPROJ | Deproject-rhythm-to-pattern UI | module-local | SLICE | 3 |
| BEATTHIS | Beat This! small0 worker | pure-new | WORKER0 | 3 |
| COVERAGE | Explained-energy coverage view + comparison objects | module-local | TILES (soft) | 4 |
| PANES | Workspace descriptors; dockable production panes | module-local (UI) | ENVELOPE (soft) | 4 |
| RETIRE | Stratum retirements (persistence.rs, session arrangement, fixed atlas) | shared-struct, one per cycle | ENVELOPE (for session) | 4–5 |
| IDM | Inverse Drum Machine worker + correction UI | pure-new + module-local | BEATTHIS | 5 |
| EXPLAIN | Explain-as-expression (rhythm → pattern terms) | module-local | NOTATION, DEPROJ | 5 |
| CLI | Headless `audec` speaking commands/queries | module-local | ENVELOPE, QUERY | 5+ |
| READINGS | Comparison persistence → shareable readings | design → later | COVERAGE, ENVELOPE | 6+ |

## 3. Workstream briefs

Each brief lists goal, ground-truth anchors (files and the *names* of the
types/functions a lane must copy real signatures from at briefing time),
deliverables, gate, and risks. Gates are written for an adversarial checker.

### SLICE — sampler slice (`selection → pads → steps → audible`)

**Goal:** from a workbench time selection, create a media-pool slice asset,
a step pattern with a lane targeting it, audible playback through the live
project, and pad audition. This is roadmap gate 3's spine and the app's first
constructive loop.

**Ground truth:** `src/ui.rs` (`Workbench::toggle_playback`, selection state,
EDIT/RECONSTRUCT sidebar, `open_sequencer_editor`); `src/live_project.rs`
(`LiveProject`, `LiveProjectDomains` including `pcm: Arc<Mutex<AssetPcmMap>>`,
`compile_audition`); `src/daw_engine.rs` (`DawEngineConfig.instruments`,
`BuiltInInstrumentRoute`, `BuiltInInstrumentDefinition::Sampler`,
`SamplerParams.trigger_asset` matching `TriggerTarget::Sample`);
`src/daw_project.rs` (`ProjectBindings::bind_sequencer_sample`);
`src/assets.rs` (`AssetRegistration`, `AssetFrameRange`, provenance types);
`src/sequencer.rs` (`SequencerCommand`, `StepPattern`, `StepLane`,
`StepEvent`, `PatternDefinition`, `PatternClip`); `src/instruments.rs`
(`SampleData`, `SamplerParams`); `src/engine_regression.rs` (the proven
step-pattern→sampler→bus fixture — the lane's model of correct wiring).

**Deliverables:**
1. Slice extraction: selected span of retained PCM → derived asset registered
   with provenance recording the source asset and exact frame range, plus its
   `PcmAsset` inserted into the live PCM map.
2. Pattern bootstrap: create step pattern + lane targeting the bound sample
   at the project's adopted BPM; open the sequencer editor attached to it;
   pattern clip placed so the arrangement compiles it.
3. **Routing:** `toggle_playback` builds `DawEngineConfig.instruments` from
   `bindings.sequencer_samples` + the PCM map (sampler per bound sample,
   routed to a created "Pads" bus) instead of passing
   `DawEngineConfig::default()`. Unroutable-event diagnostics surface in the
   UI rather than being dropped.
4. Pad audition through the existing audition bus.
5. Optional if cheap: "chop selection at onsets" using existing onset
   evidence — one pad per hit.

**Gate:** on a real FLAC: select a span, press "Sample selection", program
steps, press Play, and the rendered audition audibly contains the slice at
the programmed steps. Engine diagnostics list is empty for the happy path.
`cargo test` fully green; new regression test extends `engine_regression.rs`
style (distinctive PCM values, no constant samples). Checker verifies the
audible claim by inspecting the rendered schedule's events, not the UI.

**Risks:** `ui.rs` single-writer; do not restructure the Workbench while in
there (that is PANES). Slice assets must not duplicate on repeat invocation
(content fingerprint dedup exists — use it).

### NOTATION — pattern mini-notation core

**Goal:** a pure, deterministic pattern expression language:
`"pen ~ [pen pen] pen*4"` plus combinators (`every`, `rot`, euclidean
`e(k,n)`, `degrade(p, seed)`), parsed to a serde-able term, evaluated into
existing sequencer types. No UI, no wiring — that is NOTEWIRE.

**Ground truth:** `src/sequencer.rs` (`StepPattern`, `StepLane`, `StepEvent`,
`BeatTime`, `BeatDuration`, `PPQ`, `TriggerTarget`; copy the humanization
seed-hash idiom for `degrade`). New file: `src/pattern_lang.rs`.

**Deliverables:** `PatternExpr` term type (serde, hashable);
`parse(&str) -> Result<PatternExpr, PatternParseError>` with byte-offset
errors; `eval_steps(&PatternExpr, &BTreeMap<String, TriggerTarget>,
cycle: BeatDuration, seed: u64) -> Result<StepPattern, PatternEvalError>`.
Subdivision math is exact rational against PPQ; rounding emits a typed
diagnostic (mirror `reconstruction_apply`'s `MicrotimingRoundedToTick`
philosophy). Pretty-printer round-trips: `parse(print(t)) == t`.

**Gate:** golden tests for grammar corpus incl. nesting, `~`, `*`, `!`, `@`,
alternation `<a b>`; property test that eval is deterministic and total on
parseable input; a euclidean identity test (`e(3,8)` produces the standard
tresillo offsets); NO reference to UI or live project anywhere in the module.
Checker reads the test statements for vacuity and confirms the module's
header states its negative claims (a term is not a recording; a name binding
is not an instrument identity).

### ASPECT — aspect algebra combinators

**Goal:** compositional selections as a pure value type: `span ∩ band(low) ∩
family(7)`, union/complement, `explained_by`/`residual_of` as *deferred*
constructors (they name project references; resolution comes later). This is
the noun layer every later feature (sampling, masking, query, coverage)
consumes.

**Ground truth:** coordinate types in `src/audio.rs` (`ProjectFrame`,
`FrameRange`), `src/session.rs` (`Sample`, `SampleRange`),
`src/spectral_tiles.rs` (`FrequencyRange`), `src/rhythm.rs` (`SampleSpan`,
family ids), `docs/VISION.md` §Ontology. New file: `src/aspect.rs`.

**Deliverables:** `Aspect` term type (serde, hashable, normalized form);
constructors + set algebra; evaluation to concrete time/frequency/object
extents given a resolver trait supplied by the caller; deterministic
normalization so equal aspects hash equally.

**Gate:** algebraic property tests (idempotence, commutativity, absorption,
De Morgan for complement over a bounded universe); normalization-stability
golden tests. No dependency on GPUI or live project.

### WORKER0 — model-worker protocol harness

**Goal:** the first executable pass over `src/model_worker.rs`'s contracts:
a supervisor + a *fake worker binary* that speaks the wire protocol
(`hello/load_model/analyze/progress/complete/error/cancel`), staged atomic
artifact outputs, cancellation, crash/OOM containment, cache identity. The
point is to debug the protocol with zero model risk before BEATTHIS.

**Ground truth:** `src/model_worker.rs` (all message/manifest/cache types),
`src/model_registry.rs` (hash-verification path), `docs/ML_MODELS.md`
§Worker and artifact requirements.

**Deliverables:** a small `audec-fake-worker` binary (workspace member or
`src/bin/`), supervisor round-trip tests covering: happy path, cancellation
mid-chunk, worker crash, malformed output, partial-write rejection, cache
hit/miss identity.

**Gate:** kill -9 the worker mid-job in a test and observe a typed error
plus intact staging directory; artifacts publish atomically or not at all;
no test sleeps on wall-clock timing.

### DSPUTIL — deduplicate DSP primitives

**Goal:** one home for the ~5× duplicated `rms`, `normalized_autocorrelation`,
`cosine`, `median`/percentile, parabolic interpolation across `analysis.rs`,
`rhythm.rs`, `pitch.rs`, `loom.rs`, `hpss.rs`.

**Deliverables:** `src/dsp_util.rs`; call sites migrated; duplicates deleted.
Behavior-preserving only — where implementations differ subtly (mean-removal,
epsilon choices), keep both as *named variants* rather than silently changing
results.

**Gate:** control is exact: all existing tests pass **unchanged** (bitwise
determinism tests included). Checker diffs for any numeric-behavior change
smuggled in as cleanup.

### DIALOGS — save/open/export/recovery UI

**Goal:** File actions over the finished substrate: `project_io` envelope +
`project_codecs` payloads + autosave/recovery + `export.rs` WAV export, with
dirty-state indication and recent files. Flips audec from "demo I must not
invest in" to "tool that keeps my work."

**Ground truth:** `src/project_io.rs`, `src/project_codecs.rs`,
`src/export.rs` (`WavExportRequest`, `ExportObserver`, `ExportCancellation`),
`src/live_project.rs` (`mark_saved`, `is_dirty`), `src/ui.rs` File menu area.
Prefer a new `src/file_actions.rs` for logic so the `ui.rs` diff is thin.

**Gate:** quit/reopen/relink acceptance (roadmap workflow G, reduced scope:
save, quit, reopen, identical revisions and editor state; interrupted
autosave offers a labeled recoverable copy). Export produces a WAV whose
frames equal the audition render's for the same revision. Checker performs
the interrupted-write test at each fsync/rename boundary using the existing
project_io test idioms.

### ENVELOPE — command envelope + aggregate undo (keystone)

**Goal:** one serializable `ProjectCommand` envelope wrapping domain commands
plus binding edits; `LiveProject` applies commands instead of reconciling
deep diffs; one cross-domain undo/redo stack; journal entries become
replayable. `sequencers_equal` and friends demote to debug-assert integrity
checks.

**Ground truth:** `src/daw_project.rs` (`prepare_transaction`/`commit_prepared`,
`TransactionRecord`), `src/live_project.rs` (`reconcile`), per-domain command
types in `src/sequencer.rs`, `src/mixer.rs`, `src/arrangement.rs`
(`ArrangementEditor` transactions), `src/session.rs` (`SessionCommand` —
note it belongs to the legacy stratum; do not extend it, wrap it or bypass).
`docs/DAW_ARCHITECTURE.md` §Commands.

**Sequencing note:** this is the highest-coupling change in the campaign.
Run it as a near-solo lane with at most pure-new lanes concurrent. Land in
two steps: (a) envelope type + apply path alongside reconcile, with an
equivalence assertion (apply-result == reconcile-result) in debug builds;
(b) editors emit envelopes; reconcile demoted.

**Gate:** replaying the journal from a fresh project reproduces the exact
aggregate revision and state (byte-equivalent domain snapshots); undo/redo
across a cross-domain transaction (clip move + mixer edit + binding) restores
byte-equivalent state; redo cleared on new edit; IDs never reused after undo.
Checker writes one *new* adversarial test of its own choosing.

### NMF1 / RETIRE — consolidation lanes

**NMF1:** make `nmfd.rs` with `temporal_template_length = 1` the single
solver; port `decomposition.rs` callers (`analysis.rs` components) and delete
it. Gate: existing component hypotheses tests pass with documented, reviewed
tolerance or exact equivalence; no new public surface.

**RETIRE (one per cycle, in order):**
1. `persistence.rs` → absorbed by `project_io`/`project_codecs` (port the
   unknown-record preservation tests before deletion).
2. `session.rs` arrangement model → `arrangement.rs` (the
   `LegacyIdentityArchive` in `daw_project.rs` is the template for preserving
   analytic identities; `project.rs`'s AIR seeding migrates).
3. Fixed 1200×216 atlas in `analysis.rs` → `spectral_tiles` everywhere
   (the workbench atlas already partially migrated; finish and delete the
   fixed-size constants).

Each retirement is a shared-struct lane: solo writer, whole-tree gate.

### QUERY — AIR query combinators

**Goal:** typed, pure query combinators over `src/ontology.rs` (objects,
spans, evidence edges, hypotheses, provenance): filter/join/path predicates
with derivations retained on every result ("this fact because these facts").
Datalog-ish semantics, combinator API first, no surface syntax yet.

**Gate:** provenance completeness — every derived result can name its input
facts; determinism; termination by construction (stratified, no recursion in
v1). Property tests over synthetic AIR graphs.

### NOTEWIRE — mini-notation wiring + term provenance

**Goal:** the first *visible* language: an expression field in the sequencer
editor; evaluation into the live pattern via commands; the source term stored
in `PatternDefinition` as origin metadata (`PatternOrigin::Expression
{ source, term_hash, diverged }`); hand edits set `diverged` instead of
lying. Codecs round-trip the new field; unknown-field preservation holds.

**Class:** shared-struct (touches `PatternDefinition` + codecs). Single
writer; whole-tree gate; coordinate with ENVELOPE if concurrent.

**Gate:** type expression → hear it on next Play; edit grid → `diverged`
flags; save/reopen preserves term and flag; old project files still load.

### TILES — incremental render, edit-while-looping

**Goal:** content-address render products by
`(bus, frame_tile, relevant domain generations)` using the per-domain
generation counters already in `ProjectRevisions`; a background scheduler
renders dirty tiles ahead of the playhead; Play swaps refreshed tiles at tile
boundaries instead of re-rendering the whole project. Target experience: loop
a bar, mute an event, hear the change on the next pass without stopping.

**Ground truth:** `src/daw_engine.rs` (`DawEngineSchedule`, windowed render),
`src/daw_render.rs` (`RenderWindow`), `src/live_project.rs`
(`compile_engine`), `src/ui.rs` `toggle_playback` (the current bounce path),
`src/spectral_tiles.rs` (the cache-key discipline to imitate).

**Gate:** an edit inside the loop is audible on the following loop pass with
transport uninterrupted; an edit *outside* the loop invalidates no in-loop
tiles (cache-hit assertion); full-project bounce equals tile-concatenated
bounce byte-exactly for the same revision (the null test that keeps one
engine).

### DEPROJ — deproject-rhythm-to-pattern UI

**Goal:** the first shipped reverse→forward promotion: from the rhythm lens,
select a family/pattern hypothesis, preview "Deproject to pattern," and
apply through `reconstruction_apply` into ordinary editable material with
evidence links, original microtiming, and a revert path. UX contract is
UX_WORKFLOW_AUDIT test J.

**Ground truth:** `src/reconstruction.rs` (proposal types),
`src/reconstruction_apply.rs` (`ReconstructionApplicationPlan::prepare`,
receipt/diagnostics types), `src/rhythm.rs` (`EventFamilyHypothesis`,
`PatternHypothesis`, `BeatPhaseHypothesis`), rhythm lens in `src/ui.rs`.

**Gate:** accept creates editable lanes whose steps land on evidence spans;
no family is auto-named; reject/revert restores byte-equivalent state;
residual audition reflects the promotion immediately (with SLICE routing).

### BEATTHIS / IDM — the ML ladder

**BEATTHIS:** first real worker per ML_MODELS locks (`beat-this-rten-small-1.0.0`,
CPU, pinned hashes); logits preserved before events; results land as ranked
AIR claims beside (never replacing) native tempo hypotheses. Gate: offline
install → repeatable inference → cancellation/crash tests green → fixture
golden with recorded hashes.

**IDM:** Inverse Drum Machine adapter (44.1k mono, 9 lanes, onset/velocity +
one-shots + Wiener masks as distinct sidecars), with the authors' manual
onset-correction affordance as a primary UI, and the six-kit training caveat
displayed. Depends on worker maturity, not on separation models.

### COVERAGE / PANES / EXPLAIN / CLI / READINGS — later cycles, sketched

- **COVERAGE:** explained-energy over time×frequency as a heatmap lane;
  per-object `Explains` trait (render alone / evidence / subtract) unifying
  the Original|Construction|Residual strip; comparison objects persist with
  revision + render fingerprints (UX audit friction #7).
- **PANES:** workspace descriptor/instance IDs replace the six-singleton
  enum; production editors dock; per DAW_UI_BLUEPRINT §5. Runs best after
  ENVELOPE so panes bind to command targets, not entities.
- **EXPLAIN:** rhythm hypotheses emitted *as pattern-language terms* with fit
  + residual (`e(3,8)` as explanation). Ranked by description length. The
  flagship fusion of both directions.
- **CLI:** headless `audec` speaking the command/query protocol —
  platformization; external general-purpose scripting lives here, never
  in-process.
- **READINGS:** persistent, diffable, shareable interpretation bundles.
  Design document first; depends on COVERAGE + ENVELOPE.

## 4. Cycle schedule

Integration order within a cycle: pure-new lanes land anytime; module-local
lanes land after their module's tests pass; the single shared-struct lane (if
any) lands last, followed by whole-tree build + full suite + checker audit +
a manual smoke run of the packaged app on real material.

| Cycle | Lanes (parallel) | Solo / notes |
| --- | --- | --- |
| 0 | interface groundwork (done: this plan + COMMAND_ENVELOPE.md, LANGUAGES.md, RENDER_TILES.md) | design corpus committed before any impl lane launches |
| 1 | SLICE (ui writer) ∥ NOTATION ∥ ASPECT ∥ WORKER0 ∥ DSPUTIL | no shared-struct lanes; five disjoint file sets |
| 2 | DIALOGS (ui writer) ∥ NMF1 ∥ QUERY | ENVELOPE runs near-solo alongside pure lanes only |
| 3 | TILES ∥ DEPROJ ∥ BEATTHIS | SPLIT (workspace crates) runs solo *between* 2 and 3; NOTEWIRE lands as cycle-3's shared-struct lane |
| 4 | COVERAGE ∥ PANES ∥ RETIRE(persistence) | |
| 5 | IDM ∥ EXPLAIN ∥ RETIRE(session) | CLI design starts |
| 6+ | CLI ∥ RETIRE(atlas) ∥ READINGS design | HTDemucs enters only after IDM/BEATTHIS goldens hold |

**SPLIT note:** the workspace split (`audec-dsp`, `audec-domains`,
`audec-bridge`, `audec-app`, worker binaries) is mechanical but churns every
import; it must run with zero concurrent lanes and lands as pure file moves +
`use` rewrites with no behavior change (control: full suite green, `git diff
--stat` shows only moves/imports/Cargo metadata).

## 5. Standing guardrails (what this campaign refuses)

- No second audio engine. Realtime-callback work is deferred with recording.
- No in-process general-purpose scripting language, at any cycle.
- No hypothesis silently promoted to a source identity, including by an ML
  worker's label vocabulary.
- No lane relaxes a determinism test to make its feature fit.
- No new persistence format; extend the envelope/codec era only.
- No parallel writers on `ui.rs`, `live_project.rs`, `daw_project.rs`, or any
  file another active lane owns.

The campaign's design test, extending the repo's own: every mark answers
*what evidence is this, how was it derived, can I hear it, what happens if I
edit it* — and now also: **what term denotes it, and what command produced
it?**
