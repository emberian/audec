# Deprojection programs: evidence into editable source

Status: executable foundation, 2026-08-31. The concrete types live in
`src/deprojection_program.rs`. This document defines the seam between Audec's
analyzers/model workers and the one authoritative DAW renderer. It complements
`ML_MODELS.md`, `LANGUAGES.md`, and `COVERAGE.md`; it does not replace their
model locks, term semantics, or signal equations.

## The contract

Audec decompiles a selected source span into a ranked set of **source
programs**:

```text
exact material span
  -> immutable source claims and native measurements
  -> competing editable terms
  -> one explicit constructive program
  -> the normal DAW compiler and bounce renderer
  -> construction, residual, excess, and a ranking objective
```

A program is not a recovered session and a rank is not a posterior
probability. The source may admit many musically useful programs. No searcher,
separator, or label silently chooses one for the user.

The project equation stays:

```text
source = render(source_program) + residual
```

Residual is deliberately absent from `SourceProgram`. It is always derived
after rendering. Otherwise a candidate could include literal source plus a
residual layer and obtain a meaningless perfect score.

## 1. Source claims

`SourceClaim` is the model-neutral ingress. It pins decoded-material SHA-256,
exact half-open frame span, rate/channels, producer recipe, immutable output
digest, and optional model-worker claim/output identity. A friendly label such
as `drums` or `vocal` remains model-authored metadata.

Every audio estimate has one mathematical contract:

- `JointAdditive`: the model produced a joint set whose sum error was actually
  measured within the declared bound.
- `AdditiveTargetWithResidual`: one target and a named residual reconstruct the
  input.
- `Overlapping`: estimates can explain the same energy and are never
  auto-summed.
- `Generative`: plausible resynthesis, visibly distinct from extracted PCM.
- `Measurement`: maps, embeddings, labels, events, or other non-audio output.
- `LiteralMaterial`: an exact source citation, not separation.

An `Overlapping` or `Generative` claim may still participate in an explicitly
authored hypothesis. The graph simply refuses to manufacture a joint-stem
claim from it. Excess coverage then exposes double-accounted or invented
energy.

Raw NMF/NMFD factors are measurements, not audio claims. Without a
phase-preserving mask/PCM artifact they may seed family search or an automation
curve, but cannot compile into an audible source.

## 2. Editable terms and programs

The first target languages are deliberately small and typed:

- exact sample slices;
- pattern expressions with symbolic voice slots;
- pitch/control curve expressions in physical source time;
- note gestures with retained bends and gaps;
- editable preset/patch candidates;
- exact-audio references as an explicit high-description-cost fallback.

Pattern search uses the existing `rhythm_explanation` implementation. It
already emits deterministic competing `PatternExpr` alternatives, MDL-like
description ranks, derivations, grid diagnostics, and exact-audio fallbacks.
The deprojection layer wraps every alternative without dropping rejected or
literal cases.

Pattern voices remain symbolic while searching. `fam4` is not a sampler ID and
certainly not a kick. It becomes an executable voice only when a later compiler
binds it to a sample citation, source claim, synth patch, or explicitly chosen
instrument. `SourceProgram::compile_refusals` makes unresolved families visible
instead of rendering them as silence.

Pitch modulation compiles similarly:

- measured vibrato becomes an editable sine LFO candidate with
  `depth = extent / 2`;
- a glide becomes a line candidate;
- the evidence locator survives alongside the term;
- neither term asserts that an LFO or pitch envelope existed in the original
  patch.

Measured `rate_hz` must be evaluated in physical source time. The helper
`evaluate_curve_at_source_frame` does that now. The current general
`curve_lang::compile_curve` uses a nominal quarter-note second and must not be
used directly for physical-Hz evidence until it accepts a tempo/time map.

`SourceProgram` is an explicit constructive sum of root terms plus retained
support terms, citations, and derivations. It is content-addressable and has no
project-local instrument IDs. The eventual `SourceProgramCompiler` boundary
will:

1. resolve symbolic voices into typed assets/instruments;
2. lower patterns with real placement cycle indices;
3. allocate project-local IDs only in a prepared command envelope;
4. freeze the existing `DawEngineSchedule`;
5. return both a frozen explanation renderer and the exact atomic promotion
   commands.

Preview and promoted-project renders then null by construction. There is no
second synthesis engine.

## 3. The content-addressed computation DAG

`DeprojectionPlan` is a deterministic topological list of content-addressed
nodes. The initial vocabulary is:

- optional isolated model claims;
- native rhythm, pitch, and recurring-component analysis per source claim;
- event-family fusion;
- pattern and curve synthesis;
- construction rendering through the DAW;
- residual/excess measurement.

Nodes at the same dependency frontier can execute concurrently. Their identity
depends on exact source claims, recipe digests, dependencies, and declared
outputs. UI pane identity, job number, path, and scheduling order do not affect
the hash.

`DeprojectionRunGuard` gives native and model tasks the same generation law:
starting a new run cancels the previous generation; late results remain valid
cache material but cannot publish into the current session. The existing model
worker remains the process boundary and `RenderCancellation` remains the
cooperative in-process boundary.

## 4. Search ranking versus final ranking

Search may rank provisional programs using timing error, pitch error,
description length, and analyzer support. These are priors. In particular:

- `rhythm_explanation::ExplanationFit.energy_fit` is onset-novelty fit;
- `ReconstructionProposal::estimated_coverage` is span coverage;
- neither is reconstruction coverage.

Final ranking happens only after an exact DAW render. `score_deprojection`
combines the existing honest coverage measurements:

```text
residual_ratio = residual_power / source_power
objective = residual_weight * residual_ratio
          + excess_weight * excess_ratio
          + description_byte_weight * canonical_term_bytes
          + parameter_weight * free_parameters
          + assumption_weight * assumption_penalty
          - evidence_credit * evidence_support
```

Minimum is better. Raw residual, excess, structural cost, evidence support, and
assumption penalty remain visible separately. There is no hidden gain fitting;
`suggested_gain` is a diagnostic and applying it is an edit. Deterministic
ordering uses `total_cmp` and content ID tie-breaks.

The next refinement should quantize only the ordering key to bounded integer
units (for example residual/excess ppm and description millibits) while keeping
the raw floating measurements. That will make portable cross-platform ranking
even less sensitive to tiny FFT differences.

## 5. Model-worker authority chain

The current worker substrate is strong: process isolation, JSONL control with
file-based PCM, cache leases, verified immutable artifacts, atomic directory
publication, typed output kinds/additivity/backlinks, cooperative cancellation,
and process escalation already exist.

The live service nevertheless has one important split-brain seam. The
serde-free `model_worker::SeparationRequest` can derive the correct canonical
cache key, but `ModelTaskRecipe` still accepts a caller-provided cache string,
claimed material hash, and in-memory `ClaimPublication`. Those values can
disagree. A cache hit can therefore be reinterpreted under metadata that was
not bound into the cached recipe.

The authoritative chain should become:

```text
ValidatedInferenceRecipe -> StoredResultBundle -> ModelClaimBundle
```

### Recipe authority

A validated recipe includes:

- manifest hash and every executable dependency/adapter/runtime fingerprint;
- actual material digest, span, audio format, and preprocessing digest;
- stable named auxiliary input slots;
- canonical effective parameters and explicit random seed;
- named output contracts: kind, media type, schema, time base, additivity, and
  model vocabulary.

The service—not its caller—computes the recipe ID and verifies streamed input
bytes against the declared material digest before launch. Job IDs and sandbox
paths are routing data only.

### Result authority

The canonical recipe and runtime attestation are persisted next to every
result. Worker artifacts carry `output_name`; publication matches by name, not
vector position. A result is rejected for missing/duplicate/wrong-kind output,
time-base/additivity mismatch, nonexistent residual reference, or source
backlink outside the recipe inputs.

`ModelClaimBundle` derives solely from that authenticated stored bundle. Claim
identity must distinguish the recipe from the concrete result bundle so
hardware nondeterminism and imported readings cannot overwrite one another.

### Cancellation law

The task lifecycle should make the publication boundary explicit:

```text
Queued -> Starting -> Running -> Prepared -> Publishing -> Published
                         |           |
                         +-> CancelRequested -> Cancelled
```

Before `Publishing`, cancellation wins and prepared output is discarded from
publication. At/after `Publishing`, cancellation returns `TooLate` and the task
finishes Published. A late `Complete` after a cancel request is discarded as a
typed cancellation, not treated as a protocol-corrupting event. Deadlines then
escalate cooperative cancel to TERM/kill/reap and abandon every lease.

The UI pump must be nonblocking. Historical tasks should retain content
handles, not cloned PCM buffers. Busy identical recipe IDs should join the
single flight rather than fail as unrelated work.

### Isolation wording

The current boundary is out-of-process crash isolation, not yet a capability
sandbox: the child inherits environment/network/filesystem access. Before
calling it sandboxed, add an explicit policy with cleared/allowlisted
environment, denied network by default, read-only verified model/input
capabilities, staging-only writes, bounded RSS/output/JSONL/stderr, wall and
cancel timeouts, and a platform launcher that enforces them.

## 6. Convergence order

1. Register `deprojection_program` and use its DAG/run guard around the current
   native synthesis and comparison flow.
2. Add a DAW-backed `SourceProgramCompiler`; bind rhythm families to exact
   sample/source candidates and prove preview equals promoted render.
3. Make the serde-free canonical inference recipe authoritative in the live
   task service; verify input bytes and persist the recipe.
4. Name and validate worker outputs; construct claims from stored bundles,
   never fresh caller publication metadata.
5. Make worker pumping nonblocking and implement the cancellation/publication
   race law, timeouts, real queue/single-flight, and correct staging recovery.
6. Add runtime attestation and enforced platform resource capabilities.
7. Add codecs for source programs, artifact-qualified local evidence, scores,
   and divergence. Render caches remain ephemeral.

## 7. Adversarial gates

- Input/order permutations produce identical normalized programs, IDs, and
  ranking; scheduling order never enters a cache key.
- Anonymous rhythm families survive synthesis and promotion without becoming
  kick/snare identities absent attributed label evidence.
- Pattern alternatives lower through the real `DawEngineSchedule`; preview and
  promoted project render identically over the same extent.
- A hand edit marks pattern/curve origin diverged and regeneration requires an
  explicit overwrite command.
- A measured 6 Hz curve remains six physical cycles per second at every tempo.
- Pitch gaps remain gaps; octave alternatives remain competing programs.
- Raw NMF/NMFD factors refuse audio compilation; a phase-preserving claim can
  compile.
- Joint additive outputs group only after measured bound validation;
  overlapping estimates never auto-group.
- Summed overlap lights excess; omitted material lights residual; over-gain
  cannot hide behind clamped explained energy.
- Exact-audio fallback stays audible and visibly literal with its byte cost.
- Missing artifacts leave durable unresolved terms and typed compile errors.
- Cancellation publishes no partial program or command set; late completions
  fail the generation guard.
- Claimed material hash mismatch fails before worker launch. Changing seed,
  resampler, adapter, runtime, auxiliary model, or output schema changes recipe
  identity.
- Worker artifact order is irrelevant; output contract mismatch is rejected.
- Save/reopen round-trips terms, evidence, derivations, raw scores, selection,
  and divergence while leaving render caches disposable.
