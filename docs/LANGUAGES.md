# The language stack: terms for selecting, generating, querying, explaining

Status: normative interface design, 2026-08-31, against `main` at `dbdda7b`.
Ground truth for the ASPECT, NOTATION, NOTEWIRE, QUERY, and EXPLAIN
workstreams in [SWARM_PLAN.md](SWARM_PLAN.md). Commands — the verb layer —
are specified separately in [COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md).

## Design rules (all layers)

1. **A language is a data type plus a pure evaluator plus golden tests.**
   Surface syntax is a thin, late, separate layer over the term type.
2. **Terms are serde-serializable, content-hashable, and normalizable.**
   Equal meaning should reach equal normal form; equal normal form must hash
   equally.
3. **Determinism is total.** Every stochastic operator takes an explicit seed
   and hashes stable identities (the idiom `sequencer.rs` already uses for
   humanization). No wall clock, no ambient RNG, no iteration-order effects.
4. **Anything that changes the project passes through the command envelope;
   anything that computes is a pure term.** No in-process general-purpose
   scripting, at any layer, ever. External tools speak the protocol (§6).
5. **Terms carry negative claims like modules do.** A pattern term is not a
   recording; a name binding is not an instrument identity; a query result is
   evidence-linked, not asserted truth.
6. **Provenance is part of the value.** Generated artifacts record the term
   that generated them; hand edits set a `diverged` flag rather than lying.

---

## 1. Aspect algebra (nouns) — `src/aspect.rs`

Compositional selections over every coordinate system audec has. Every later
feature (sampling, masking, query scoping, coverage, audition) takes an
`Aspect` instead of inventing private selection state.

```rust
/// A pure selection term. Time is half-open signed project frames; frequency
/// is Hz with an explicit scale tag; objects/families are typed references
/// resolved by the caller-supplied resolver.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Aspect {
    All,
    Time(FrameSpan),                    // { start: i64, end: i64 }, start < end
    Band(BandSpan),                     // { min_hz: f32, max_hz: f32 }
    Channels(ChannelMask),              // bitset over source channels
    Family { analysis: AnalysisRef, id: usize },
    Object(ontology::ObjectId),
    Union(Vec<Aspect>),
    Intersect(Vec<Aspect>),
    Complement(Box<Aspect>),            // relative to a declared universe
    /// Deferred references: name project entities; resolution happens at
    /// evaluation against a resolver, never inside the term.
    ExplainedBy(ExplanationRef),
    ResidualOf(ExplanationRef),
}

/// Canonical form: unions/intersections flattened and sorted, sibling
/// time/band spans interval-merged, `All`/empty absorbed, double complement
/// eliminated. `normalize` is idempotent; equal-meaning terms on the same
/// primitive set reach the same normal form.
pub fn normalize(aspect: Aspect) -> Aspect;

/// Evaluation contract. The resolver is the only impure-looking edge and it
/// is read-only; `aspect.rs` itself never touches project state.
pub trait AspectResolver {
    fn universe(&self) -> ConcreteAspect;
    fn family_spans(&self, analysis: &AnalysisRef, id: usize)
        -> Option<Vec<FrameSpan>>;
    fn object_extent(&self, object: ontology::ObjectId)
        -> Option<ConcreteAspect>;
    fn explanation_extent(&self, reference: &ExplanationRef)
        -> Option<ConcreteAspect>;
}

/// The evaluated shape: disjoint sorted time spans, disjoint sorted bands,
/// channel mask, and the object set that contributed.
pub struct ConcreteAspect { /* time: Vec<FrameSpan>, bands: …, … */ }

pub fn evaluate(
    aspect: &Aspect,
    resolver: &dyn AspectResolver,
) -> Result<ConcreteAspect, AspectError>;
```

**Laws (property-tested):** union/intersect commutativity + associativity +
idempotence, absorption, De Morgan over a bounded universe, `normalize`
stability, `evaluate(normalize(a)) == evaluate(a)`.

**v1 boundary:** `Band` participates in normalization and evaluation but only
time/channel components are *executable* (slicing PCM); band-limited
execution arrives with mask rendering. Evaluation of an unresolvable
reference is a typed error, never a silent `All`.

---

## 2. Pattern mini-notation (generators) — `src/pattern_lang.rs`

A pure, deterministic pattern language evaluating into the real sequencer
types. TidalCycles-adjacent in spirit, audec-typed in practice.

### Surface grammar (one cycle per expression)

```text
pattern     := step+
step        := element modifier*
element     := name | rest | group | alternation
name        := [a-z][a-z0-9_-]* (":" uint)?      -- binding, optional variant
rest        := "~"
group       := "[" pattern "]"                   -- nested subdivision
alternation := "<" pattern ">"                   -- one element per cycle, round-robin
modifier    := "*" uint                          -- subdivide into n repeats
             | "!" uint                          -- replicate as n siblings
             | "@" number                        -- relative width (default 1)
             | "?" number?                       -- probability (default 0.5)
```

Combinators are function-shaped terms above the mini-notation (constructed in
code or by a later surface syntax; the mini-notation string parses into the
same term type):

```text
seq(p…)   stack(p…)   every(n, f, p)   rot(k, p)   e(k, n [, rot])
swing(amount, p)   gain(g, p)   degrade(prob, p)   fast(n, p)   slow(n, p)
```

### Term and evaluator

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PatternExpr { /* mirrors the grammar; no strings-as-code anywhere */ }

pub fn parse(source: &str) -> Result<PatternExpr, PatternParseError>;   // byte-offset errors
pub fn print(expr: &PatternExpr) -> String;                             // parse(print(t)) == t
pub fn term_hash(expr: &PatternExpr) -> ContentHash;

pub struct EvalContext<'a> {
    /// Name → trigger binding. One step lane per distinct bound name.
    pub bindings: &'a BTreeMap<String, sequencer::TriggerTarget>,
    /// One cycle's musical length.
    pub cycle: sequencer::BeatDuration,
    /// Seed for `?` and `degrade`, combined with stable event identities.
    pub seed: u64,
    /// Which cycle index alternations select from.
    pub cycle_index: u64,
}

pub struct EvalOutput {
    pub pattern: sequencer::StepPattern,
    pub diagnostics: Vec<PatternEvalDiagnostic>,   // e.g. RoundedToTick { ticks_error }
}

pub fn eval_steps(
    expr: &PatternExpr,
    context: &EvalContext<'_>,
) -> Result<EvalOutput, PatternEvalError>;
```

### Placement semantics (exact, against the real types)

- Widths partition the cycle by exact rational arithmetic over PPQ (`PPQ =
  960`, `BeatTime(i64)`, `BeatDuration(u64)`). A non-integral tick boundary
  rounds to nearest and emits `RoundedToTick` — mirroring
  `reconstruction_apply`'s `MicrotimingRoundedToTick` philosophy.
- **Grid choice:** `StepPattern` is a grid (`resolution` + `steps:
  BTreeMap<u32, StepEvent>` per lane) with per-step `micro_offset: i32`
  ticks. The evaluator selects `resolution` = the coarsest regular grid that
  hits every event exactly, if it needs ≤ 64 steps per cycle; otherwise
  `cycle / 16` with residues encoded in `micro_offset`. The rule is
  deterministic and stated in the module docs.
- `*n` on a leaf occupying one output grid step maps to `StepEvent.ratchets
  = n` (the field counts total hits including the first). Otherwise `*n`
  expands into subdivided steps.
- `?p` sets `StepEvent.probability = p`; realization stays in the sequencer's
  existing seeded scheduler — the language never rolls dice itself.
- `name:k` selects variant `k` of a binding when the binding table carries
  variants; unbound names are an eval error naming the identifier.
- `stack` merges lanes; colliding steps on the same lane/tick are an eval
  error (no silent last-writer).

### Provenance: terms ride in the definition (NOTEWIRE)

```rust
/// New field on sequencer::PatternDefinition (shared-struct change; codecs
/// must round-trip it and default old files to Authored).
pub enum PatternOrigin {
    Authored,
    Expression {
        source: String,
        term_hash: ContentHash,
        bindings_hash: ContentHash,
        /// Set (never silently cleared) when the realized pattern is edited
        /// by hand after generation.
        diverged: bool,
    },
    Deprojected {
        proposal: reconstruction::ReconstructionProposalId,
        diverged: bool,
    },
}
```

Regenerating from a non-diverged expression is loss-free; regenerating over a
diverged pattern requires explicit confirmation (Make-Unique philosophy).

---

## 3. Curve expressions (generators, control-rate) — `src/curve_lang.rs`

Same discipline, targeting automation. Terms compile to `AutomationLane`
points with explicit `SegmentShape`s at a declared control resolution.

```rust
pub enum CurveExpr {
    Const(f64),
    Line { from: f64, to: f64 },
    Lfo { shape: LfoShape, rate_hz: f64, depth: f64, phase: f64 },
    Env { attack: f64, decay: f64, sustain: f64, release: f64 },
    Sum(Vec<CurveExpr>),
    Scale { input: Box<CurveExpr>, multiply: f64, add: f64 },
    Clamp { input: Box<CurveExpr>, min: f64, max: f64 },
    /// Pretty-printed evidence: pitch.rs's ModulationEvidence::Vibrato
    /// { rate_hz, extent_semitones } becomes Lfo { sine, rate_hz,
    /// depth: extent/2 } with the evidence reference retained.
    FromEvidence(EvidenceRef),
}

pub fn compile_curve(
    expr: &CurveExpr,
    span: (BeatTime, BeatTime),
    control_resolution: BeatDuration,
) -> Result<Vec<automation::AutomationPoint>, CurveError>;
```

Curve terms ride on lanes exactly as `PatternOrigin` rides on patterns
(an `origin` field with a `diverged` flag; same codec obligations).

---

## 4. AIR query combinators (questions) — `src/air_query.rs`

Typed, pure, stratified queries over `ontology::AuditoryIr`, every result
carrying its derivation. Combinator API first; surface syntax only after the
combinators stabilize.

```rust
/// Read-only fact views. ontology.rs stays the owner of truth.
pub trait AirFacts {
    fn objects(&self) -> Box<dyn Iterator<Item = FactRef> + '_>;
    fn evidence_of(&self, fact: FactRef) -> Vec<FactRef>;
    fn relations(&self, kind: RelationKind) -> Box<dyn Iterator<Item = (FactRef, FactRef)> + '_>;
    fn extent(&self, fact: FactRef) -> Option<aspect::ConcreteAspect>;
}

pub enum Query {
    Kind(FactKind),
    Within(aspect::Aspect),
    Related { kind: RelationKind, to: Box<Query> },
    NotExplainedBy(ExplanationRef),
    And(Vec<Query>), Or(Vec<Query>), Not(Box<Query>),
}

pub struct Derivation {
    pub rule: &'static str,
    pub premises: Vec<FactRef>,
}

pub fn run(
    query: &Query,
    facts: &dyn AirFacts,
    resolver: &dyn aspect::AspectResolver,
) -> Result<Vec<(FactRef, Derivation)>, QueryError>;
```

**Guarantees:** deterministic result order (stable sort by typed ID);
termination by construction (non-recursive v1); provenance completeness —
every returned fact names the premises that admitted it. `Not` is evaluated
against the finite fact universe only (stratified negation).

---

## 5. Explain-as-expression (the fusion) — EXPLAIN workstream

Deprojection emits *terms in the generator languages*, ranked by description
length, with residual fit attached. A short program that regenerates the
evidence is a good explanation.

```rust
pub struct PatternExplanation {
    pub expr: pattern_lang::PatternExpr,
    pub bindings: BTreeMap<String, sequencer::TriggerTarget>,
    /// Family → binding-name association is anonymous ("fam4"), never an
    /// instrument label.
    pub families: BTreeMap<usize, String>,
    pub fit: ExplanationFit,          // explained energy, timing error stats
    pub description_len: u32,         // serialized term size, the MDL rank key
    pub evidence: Vec<reconstruction::ReconstructionEvidenceId>,
}

pub fn explain_rhythm(
    deprojection: &rhythm::RhythmDeprojection,
    families: &[usize],
    budget: ExplainBudget,            // bounded search, deterministic
) -> Vec<PatternExplanation>;         // ranked: description_len, then fit
```

Accepting an explanation applies through `reconstruction_apply` and records
`PatternOrigin::Expression` — the same provenance as hand-typed notation, so
the editor cannot tell (and need not care) whether a term came from a human
or from search. That symmetry is the point.

---

## 6. Protocol frame (external tools; later, but fixed now)

Headless `audec` speaks framed JSON over stdio/socket: versioned envelope
`{ "audec_protocol": 1, "op": …, "body": … }` with ops `apply`
(a `CommandEnvelope`), `query` (a `Query` + `Aspect` scope), `render`
(an `Aspect` + destination + format), `describe` (schema/version
introspection). Unknown major version is refused; unknown op is refused;
unknown *fields* are ignored. General-purpose scripting lives on the far
side of this boundary, in whatever language anyone likes, forever.
