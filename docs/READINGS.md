# Readings: shareable, diffable interpretations of a recording

Status: design exploration, 2026-08-31, against `main` at `79a80d9`.
Normative core for the READINGS workstream in [SWARM_PLAN.md](SWARM_PLAN.md);
the final section is explicitly speculative. Related contracts:
[VISION.md](VISION.md) (ontology, phenomenotechnique),
[COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md) (import mechanics),
[COVERAGE.md](COVERAGE.md) (comparisons), [LANGUAGES.md](LANGUAGES.md)
(terms as portable explanation), [ML_MODELS.md](ML_MODELS.md) (provenance
discipline for model-derived claims).

An audec project is not a song file; it is a claim graph about a recording.
A **reading** is that claim graph made portable: one author's attributed,
evidence-linked, auditionable interpretation of one source, exchangeable
with people who have their own copy of the same recording. Readings are the
mechanism by which audec becomes a communication medium rather than only a
tool.

## 1. What a reading is — and refuses to be

A reading **is**:

- an AIR subgraph: objects, spans, evidence, relations, hypothesis sets with
  their selections, all with `ontology::Provenance` intact;
- the constructive interpretation: accepted reconstruction bindings,
  authored patterns/automation with their `PatternOrigin`/curve origins
  (terms travel; they are the most compressed honest explanation we have);
- comparisons ([COVERAGE.md](COVERAGE.md)) with their metrics and
  fingerprints — the reading's own record of where it fails;
- attributed perceptual annotations and lexicon entries (§5);
- identity: the source's `assets::ContentFingerprint`(s), duration, sample
  rate, and the reading's own stable identity and version chain.

A reading **is not**:

- **a stem pack.** No source audio travels by default, ever. Constructions
  and residuals are recipes over a recording the recipient must already
  have.
- **an authority claim.** Every hypothesis remains a hypothesis with its
  alternatives; `HypothesisSelection::UserAccepted` is an attributed choice,
  not a fact. Importing a reading never auto-accepts anything.
- **a license.** A reading of a recording confers no rights to the
  recording. The format must remain useful precisely when the audio cannot
  be redistributed — that constraint is a feature, not a limitation.
- **a project backup.** Workspace layout, undo history, caches, and local
  paths do not travel.

## 2. Portable format

A sibling of the `project_io` envelope (`ProjectFile` is the model:
versioned JSON, sorted sections, verbatim-retained unknown extensions):

```text
ReadingFile
  format: "audec-reading"        version: u32
  reading_id                     stable identity, minted at creation
  parents: [reading_ref]         version-chain and merge ancestry (§4)
  author: ontology::Producer + Provenance
  source:
    fingerprints: [assets::ContentFingerprint]   // never PCM
    sample_rate, frame_count, declared_title?
  sections: [DomainSectionRecord-style payloads]
    air              // objects, evidence, relations, hypothesis sets
    constructions    // reconstruction bindings, pattern/curve terms + origins
    comparisons      // COVERAGE.md objects incl. fingerprints and metrics
    lexicon          // attributed predicate definitions (§5)
  attachments?: [derived-audio refs]             // §3, explicit opt-in only
  extensions: retained verbatim
```

ID discipline: every entity in a reading is addressed as
`(reading_id, local_id)`. `ontology.rs` already commits to explicit,
importer-preservable IDs; readings extend that to a two-level namespace so
**import never renumbers and two readings can never collide**. An entity
imported from reading R keeps its `(R, id)` name forever, including through
later merges.

## 3. What travels, what a recipient can verify

Three verification tiers, all explicit in the UI:

1. **Without the recording.** The claim graph, terms, comparisons'
   *recorded* metrics, and lexicon are readable and internally checkable:
   hash integrity, evidence-chain completeness (every claim reaches
   evidence; every `Derived` premise resolves), schema validity. Nothing is
   audible. The reading renders as a document, not a session.
2. **With a fingerprint-matching copy of the recording.** Full standing:
   transforms re-run (they are content-addressed recipes), constructions
   render, residuals and coverage recompute, and recomputed
   `GoldenFingerprint`s are checked against the reading's recorded ones
   within declared slop. A reading whose fingerprints reproduce is
   *replicated*, in the scientific sense, on the recipient's machine.
3. **With a mismatched copy** (different master, lossy transcode): the
   import is refused with a specific diagnosis (fingerprint mismatch, frame
   count delta), never silently accepted. Tolerant alignment across masters
   is speculative (§7), not v1.

**Attachments** (derived audio: construction bounces, residual previews)
are an explicit per-export user choice, default off, each carrying
provenance marking it derived-from-reading — so tier-1 recipients can hear
*something* when the author chooses, without the format ever quietly
becoming a stem pack.

## 4. Diff and merge

Two readings of the same source share a coordinate system (source frames +
content fingerprint), which is what makes structural diff meaningful.

- **Diff** is object-level and typed: added/removed/changed entities per
  section, extent overlaps between the two readings' objects, hypothesis
  sets asking the same `question` with different alternatives or
  selections, comparisons over overlapping spans with their coverage
  deltas. A diff is a report, never an auto-resolution.
- **Merge is import-as-envelope.** Importing reading B into a project
  holding reading A compiles to
  [COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md) commands: atomic, validated,
  undoable, journaled. B's entities arrive under their `(B, id)` names.
  Where A and B answer the same question, the result is one
  `HypothesisSet` holding both alternatives — the ontology's existing
  competing-hypothesis machinery *is* the merge semantics. Selection state
  is per-user and never merged.
- **Three-way merge** (shared parent in the version chain) auto-carries
  changes only one side made; both-sides-changed becomes coexisting
  alternatives with attribution. There is deliberately no textual-conflict
  state: the failure mode of merging interpretations is silent
  overwriting, and the design makes it unrepresentable.
- **Merge is refused** when source fingerprints differ, when the schema
  major version is unknown, or when envelope preconditions fail (a
  reading's construction referencing project state that cannot be
  reconstructed) — refusal names the exact entity.

## 5. Attribution and the lexicon

The phenomenotechnique layer (VISION.md; roadmap M6) lands here as data:

```text
LexiconEntry
  term: "mucusy" | "approaching" | "cold" | …
  author: Producer::Human
  scope: personal                       // never universalized
  calibration: [(aspect term, rating)]  // examples the author marked
  notes?: free text
Annotation
  aspect: LANGUAGES.md aspect term      // what it is about
  term or free text                     // what was perceived
  author + Provenance                   // who and when
```

An annotation is `EvidenceKind::HumanAnnotation` with an author — the
ontology already carries the type. Queries may filter by
`feels(term, by=author)` resolving strictly against that author's lexicon
([LANGUAGES.md](LANGUAGES.md) §4 gains an author-scoped atom). Two authors'
uses of the same word are different predicates, permanently. Nothing in the
format aggregates perceptual terms across authors into a consensus value —
if that is ever wanted, it is analysis *over* readings, attributed as such.

## 6. Implications and demands on earlier workstreams

What readings make possible: pull-requests for hearing (propose a reading
delta; the recipient auditions where it fails and counter-proposes);
worked readings as teaching material (a curriculum is a sequence of
readings with rising coverage); sample-archaeology chains of custody
(evidence with provenance, not vibes); honest session recovery (masters
survive; project files die — a reading is the recovered session's truthful
form, uncertainty included).

Concrete requirements this pushes upstream:

- **ENVELOPE**: `IdClaims` must admit foreign-namespace IDs
  (`(reading_id, local_id)`) so imports replay deterministically; import
  batches need a single-transaction grouping label.
- **QUERY**: an author-scoped perceptual atom; queries over multiple
  loaded readings need reading-qualified fact references.
- **COVERAGE**: comparisons must serialize in a section readings can embed
  unchanged (one DTO, two envelopes).
- **DIALOGS**: export/import surfaces eventually; refusal diagnostics must
  be presentable, not just typed.
- **project_io**: the reading envelope reuses its atomic-write and
  unknown-retention machinery; extract the sharable core rather than
  duplicating it.
- **ontology**: entity IDs must remain stable across save/load (already a
  stated design goal — it becomes a tested invariant here).

## 7. Speculative (explicitly non-normative)

- **A fingerprint-keyed commons.** Readings indexed by source content
  fingerprint: look up a recording, find its public readings, diff them.
  The interesting governance question (whose reading is "primary"?) has a
  designed answer: nobody's; the index is a set, and union coverage
  ([COVERAGE.md](COVERAGE.md) §5) is the only aggregate worth computing.
- **Blind exchange.** Two people read the same track without seeing each
  other's work, then diff — the disagreements are the curriculum. audec
  could scaffold this as a first-class ritual.
- **Remaster-tolerant alignment.** Cross-master reading transfer via
  constellation-hash anchoring (the sample-matching machinery in
  ML_MODELS.md) with per-span confidence — genuinely useful, genuinely
  hard, explicitly not v1.
- **Literate readings.** Export a reading as a document whose cells are
  terms (aspects, patterns, queries) with rendered auditions attached —
  the notebook lens's file format falls out of the reading format.
- **Readings by non-humans, honestly.** A model-generated reading is just
  a reading whose `Producer` is an `Analyzer` — the attribution machinery
  needs zero changes to keep human and machine hearings distinguishable
  forever. That neutrality is worth protecting on purpose.
