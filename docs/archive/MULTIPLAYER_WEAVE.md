# Multiplayer without a second project truth

Status: architecture decision, 2026-09-01, against audec `9321305` and
universal-weave `8024944`.

Related contracts: [COMMAND_ENVELOPE.md](COMMAND_ENVELOPE.md),
[READINGS.md](READINGS.md),
[PRODUCT_INFORMATION_ARCHITECTURE.md](PRODUCT_INFORMATION_ARCHITECTURE.md),
and [RENDER_TILES.md](RENDER_TILES.md).

## Decision

**Use universal-weave only as an optional index and interaction model for
immutable branches, proposals, and reading ancestry. Do not make it the DAW
model, command authority, journal, asset transport, presence channel, or audio
clock. Do not add the dependency yet.**

Comprehensive multiplayer is compatible with audec, but universal-weave does
not make it comprehensive by itself. The shortest trustworthy route is:

1. keep `ProjectController` as the only authority allowed to publish a
   `DawProject` revision;
2. have a session coordinator totally order accepted `CommandBatch` values,
   applying every one through `CommandEnvelope` and the existing validated
   transaction path;
3. retain stale or conflicting submissions as attributed proposals instead of
   feeding them to a last-write-wins register;
4. synchronize media through a separate, opt-in, strong-digest content
   channel; and
5. add universal-weave behind an audec-owned graph interface only after the
   protocol, provenance, ID, and replay gates in this document pass.

This is not a rejection of weave. It is a deliberately narrow job for it. A
weave is a good way to present “these edits descended from this state, these
people explored different alternatives, and this proposal was merged.” It is
not a safe answer to “which mixer graph is allowed to feed the speakers?”

No collaboration Rust module is added with this decision. The protocol still
has material choices—hosted versus peer-authoritative operation ordering,
project-ID leasing, and local-versus-shared asset state—which would be falsely
stabilized by types today. The isolated seam below is precise enough to
implement after those gates without disturbing current product work.

## 1. The authorities that must remain singular

The live tree already has unusually strong boundaries for multiplayer:

- `command::CommandEnvelope` is an atomic, put-style, invertible aggregate
  edit. It checks the aggregate revision, exact `before` values, and explicit
  ID claims.
- `DawProject::prepare_transaction` clones the whole aggregate, applies a
  mutation, derives/validates touched domains, follows cross-domain bindings,
  and publishes nothing on failure. `commit_prepared` checks the revision a
  second time before advancing exactly one aggregate revision.
- `ProjectController` owns the authoritative aggregate, undo/redo,
  coalescing boundary, journal sequence, and immutable publication. Its
  journal records execute, undo, and redo as ordinary forward applications.
- `command_journal` is a contiguous, checksummed, append-only revision chain.
  A gap, torn suffix, bad checksum, or unknown runtime operation is not
  guessed through.
- `project_codecs` rebuilds checked domain objects without renumbering and
  persists allocator high-water marks. Unsupported or allocator-incompatible
  input is a visible error.
- render identity includes exact project generations, dependency digests,
  engine ABI/configuration, canonical block size, and performance seed.
  Determinism is graded rather than assumed.

Those properties are the collaboration kernel. Multiplayer must send work
*toward* it, not place a mutable replica beside it.

```text
participant intent / offline proposal
                 │
                 v
      authenticated session coordinator
                 │
       validate permission + codec
                 │
       lower/rebase against current head
                 │
                 v
 ProjectController::execute(CommandEnvelope)
                 │
      atomic aggregate publication
                 │
        accepted journal record
                 │
        ordered replica broadcast
                 v
       local render-plan compilation
```

The coordinator may be an owner process, a hosted service, or a later elected
peer. “Coordinator” names the sequencing role, not a required business model.
There is exactly one such role for each accepted project head at a time.

## 2. What universal-weave actually supplies

The audited universal-weave tree supplies:

- `DependentWeave`: an ordered tree whose node contents depend on their one
  parent and whose active state names a single leaf/path tip;
- `IndependentWeave`: an ordered DAG with multiple parents, moves, and an
  active path, intended for independently meaningful node contents;
- stable traversal and ordering, bookmarks, activation, split/merge
  primitives, checked Serde/rkyv deserialization, layout support, and wrappers
  for logging/counting/deduplication;
- experimental `DependentLoroWeave`: a Loro tree/map/list projection with
  version-vector update export/import and convergence tests for two and three
  virtual peers.

That is useful substrate for branch navigation. The boundaries matter more
than the feature list:

- Only the **dependent tree** has a Loro collaboration wrapper. The TODO
  explicitly waits for Loro DAG CRDT support. A merge commit with two parents
  therefore has no native collaborative weave representation today.
- Concurrent node-content and metadata updates use last-write-wins. Audec
  project puts intentionally do the opposite: an unexpected `before` rejects.
- The Loro wrapper's split and merge operations are intentionally unavailable.
- Active node and bookmarks are stored in the shared Loro document. Audec's
  focused pane, active branch, viewport, selection, and most bookmarks are
  personal presentation state, not shared project truth.
- Removing a dependent node removes its descendant subtree. Accepted audit
  history must be append-only and must never inherit that behavior.
- `LoggedWeave` actions are successful data-structure mutations, not
  transactional project commands. They contain no aggregate base revision,
  exact inverse, ID-claim discipline, change set, actor, or durable unknown
  command policy. `ActionableWeave::apply` asserts/panics when application
  fails.
- Loro node contents are opaque rkyv bytes. The audec persistence boundary is
  versioned JSON with deliberate unknown-record retention and checked public
  reconstruction. One must not replace the other.
- The Loro wrapper warns that desynchronization may yield incorrect updates or
  panics, and `update` rebuilds the weave from the document. Its own TODO still
  calls for fuller tests, fuzzing, executable contracts, and a complete review.
- The merge property tests use one counter shared outside all virtual peers,
  so generated node IDs do not collide. They prove convergence of tested weave
  operations, not offline ID allocation, audec command validity, permissions,
  provenance, hostile-input handling, or audio equivalence.
- universal-weave contains no network, authentication, authorization,
  presence, media transfer, or realtime transport protocol.

The rkyv-and-Loro feature also brings a pinned Loro version and a second
serialization regime into audec. That cost is warranted only when the graph
adapter has a real user-facing consumer.

## 3. Concrete concept mapping

| audec concept | weave role | authority and storage |
| --- | --- | --- |
| Accepted aggregate edit | Immutable node *reference* is allowed; command bytes are not mutable weave contents. | `ProjectController` and the audec journal remain authoritative. |
| Divergent edit proposal | Natural child branch from the observed accepted head. | Audec proposal store owns author, command digest, lifecycle, and payload. Weave indexes it. |
| Accepted project head | May be highlighted in a branch view. | Session sequence + resulting revision + state digest, never weave `active`. |
| Merge of edit branches | A new resolution envelope with both proposal parents in collaboration metadata. | Audec validates and accepts it as one new linear journal record. |
| Reading version | Natural immutable graph node, already carrying parent refs. | `ReadingFile` and `(reading_id, kind, local_id)` remain the identity source. |
| Competing interpretations | Sibling alternatives are an excellent weave presentation. | AIR hypothesis sets/readings retain alternatives and attribution. |
| User's active branch/bookmarks | Potential local `IndependentWeave` presentation state. | Replica-local workspace/preferences; do not write shared Loro active/bookmarks. |
| Pane layout, zoom, tool, inspector state | No shared weave role by default. | `WorkspaceViewDescriptor` and pane-local state on that replica. |
| Semantic selection / “look here” | Ephemeral presence message carrying stable `ObjectRef`/aspect. | Presence channel with expiry; never journaled. |
| Transport/playhead | Optional host-led ephemeral synchronization. | Audio/session transport service; never weave or project persistence. |
| Asset/media bytes | No weave role beyond an immutable digest reference. | Authorized content-addressed side channel. |
| Undo | New coordinator-validated inverse application. | Aggregate history/journal; never deletion of a weave node. |
| ACL, identity, membership | No weave metadata role. | Authenticated session policy log outside the project graph. |

The critical type distinction is:

```text
CollaborationNodeId != project-local domain ID != QualifiedEntityId

CollaborationNodeId     globally unique operation/proposal identity
project-local domain ID monotonic u64 interpreted inside one project lineage
QualifiedEntityId       (ReadingId, kind, local_id), never flattened
```

## 4. The audec-owned protocol seam

The collaboration protocol should be defined without any universal-weave
types in its public wire model. A future adapter can consume these records:

```text
Submission
  protocol_version
  session_id
  operation_id              // globally unique, stable across retries
  actor_id
  observed_head:
    accepted_sequence
    project_revision
    project_state_digest
  causal_parents: [operation_id]
  command_batch_digest
  durable_command_batch     // known codecs execute; unknown stay opaque
  declared_asset_refs: [strong content ref]
  client_capabilities

AcceptedOperation
  all immutable Submission identity/provenance fields
  accepted_sequence         // session total order
  base_revision
  resulting_revision        // exactly base + 1
  resulting_state_digest
  canonical_command_batch
  coordinator_id
  authorization_receipt

ProposalDisposition
  Pending | Accepted(sequence) | Rejected(reason) |
  Superseded(by_operation) | Withdrawn
```

`operation_id`, actor attribution, causal parents, and authorization do not
currently exist in `CommandEnvelope` or `CommandJournalRecord`. They must be
durable collaboration metadata, not embedded in a label and not inferred from
a Loro peer ID. A Loro `PeerID` identifies a CRDT replica, which is neither a
human identity nor permission evidence.

The protocol owns canonical bytes and digests. The weave adapter sees only an
immutable summary such as:

```text
GraphNode
  collaboration_node_id
  parent_node_ids
  kind: accepted | proposal | reading_version | resolution
  payload_digest
  display_summary
```

Changing a proposal creates a new node. No caller invokes
`get_contents_mut` on shared graph content. Deleting, splitting, moving, or
merging an accepted node is forbidden. This makes last-write-wins content and
cascading subtree removal irrelevant instead of hoping they behave like audec
history.

### Minimal adapter boundary

The first implementation seam should be an audec trait in a collaboration
crate/module, not a dependency in domain code:

```text
CollaborationGraphIndex
  observe_immutable(record) -> Result
  ancestry(node) -> [node]
  children(node) -> [node]
  heads(scope) -> [node]
  validate() -> Result
```

An in-memory audec test implementation can land first. A later
universal-weave adapter may use:

- `IndependentWeave` for a local DAG/index of accepted operations, resolution
  ancestry, and readings; or
- `DependentLoroWeave` for a restricted one-parent proposal tree only, with
  merge-parent metadata stored in the immutable audec record.

The adapter is downstream of accepted/proposal records. It never emits a
`DomainCommand`, allocates a domain ID, calls `commit_prepared`, or decides
which branch is audible.

## 5. Accepted DAW edits: convergence by sequencing, not field CRDTs

DAW state contains order-sensitive, cross-domain invariants: a clip refers to
a track and material; a pattern occurrence agrees with sequencer placement;
routing must remain acyclic; automation targets must resolve; sample rates
must agree; render invalidation must cover every audible consequence. A CRDT
can converge structurally while violating those facts.

Accepted project edits therefore converge as follows:

1. A participant builds or requests a `CommandBatch` against an observed
   accepted head.
2. The coordinator authenticates the actor, checks domain/address permission,
   resolves every required asset, and decodes only recognized command codecs.
3. If the observed head is current, it applies the envelope normally.
4. If the submission is stale, the coordinator may attempt a **semantic
   rebase** by changing only the aggregate `base_revision`, then applying the
   unchanged command term to a clone of the current aggregate. Exact `before`
   values, ID claims, domain checks, and whole-project validation still have
   to pass. This requires a public dry-run/preflight API before implementation;
   callers must not duplicate `apply_domain_command`.
5. Success assigns the next session sequence and broadcasts the exact accepted
   record. Every replica applies accepted records in that order only.
6. Failure retains the submission as a branch/proposal with a typed conflict.
   Nothing is silently overwritten, partially applied, or rendered as the
   accepted project.

Coordinator receive order is sufficient for hosted online sessions. A later
peer coordinator must use an explicit deterministic election and ordering
rule; Loro's converged tree order must not implicitly become command order.

### Conflict granularity in v1

The current put granularity is intentionally the merge granularity:

| Concurrent edits | v1 result |
| --- | --- |
| Different clips/entities, all `before` guards still match | Semantic rebase may accept both in coordinator order. |
| Same clip, even if one changes placement and one changes gain | Conflict; retain both proposals. Do not synthesize a field merge yet. |
| Same pattern definition or automation lane | Conflict at the whole put unit. |
| Track order versus track membership/change | Dry-run and full validation decide; otherwise proposal. |
| Mixer edits | Treat as whole-domain conflicts while `MixerCommand` addresses the whole domain and carries ephemeral revision guards. |
| Create/create with the same project ID | Conflict regardless of distinct content. |
| Delete versus edit of one entity | Conflict; a resolution explicitly chooses deletion, restoration, or a replacement. |
| Commands over disjoint addresses which jointly violate a binding/invariant | Full aggregate validation rejects the later proposal. |

This is conservative and understandable. Finer commands can later make more
edits commute, but multiplayer must not invent a field merge that the undo,
journal, codec, and render invalidation systems cannot express.

### Resolution and collaborative undo

A merge is a new attributed envelope built against the current accepted head.
It may choose one proposal, compose compatible commands, or author a third
result. Its collaboration metadata cites every resolved proposal; the normal
audec journal remains linear and replayable.

Undo is also a new accepted edit, not history erasure. “Undo my edit” asks the
coordinator to apply that edit's exact inverse to the current head. If later
work changed its preconditions, the inverse becomes a proposal requiring
resolution. A user cannot erase another participant's accepted operation by
moving or deleting a history node.

Coalescing stays an editor/session-local undo presentation. The append-only
journal already records every applied command before in-memory entries are
coalesced, which is the correct network behavior.

## 6. Interpretations converge differently

Interpretation is not a second spelling of DAW mutation.

- Foreign entities retain `(reading_id, kind, local_id)` permanently. A
  collaboration or weave node ID never replaces or truncates that name.
- Imported reading entities are immutable. A changed reading creates another
  reading revision with explicit parents and a strong manifest digest.
- Concurrent hypotheses answering the same question coexist in one
  attributed hypothesis set. They do not enter a last-write-wins register and
  are not treated as an error merely because they disagree.
- Selection/acceptance of a hypothesis is per-user unless someone explicitly
  authors a shareable selection claim. Import never auto-accepts it.
- Three-way reading merge carries one-sided additions and represents
  both-sides-changed interpretations as alternatives. Source fingerprint or
  schema-major mismatches remain typed refusals.
- Bringing an interpretation into the playable construction is an explicit
  import/apply envelope. Only that operation enters the accepted DAW sequence.

This is where a weave view adds the most product value soonest: ancestry,
siblings, bookmarks, and comparison are useful without allowing the graph to
choose musical truth.

## 7. Identity and offline allocation

Reading IDs are already ready for exchange: `ReadingId` is nonzero 128-bit
identity, `QualifiedEntityId` includes an open kind plus nonzero local ID, and
durable command claims retain the full foreign reading namespace. Do not map
those entities into bare project-local u64 IDs on import.

Ordinary DAW IDs are not yet ready for unconstrained offline creation. They
are independent monotonic u64 spaces with persisted high-water marks. Two
disconnected replicas starting at the same checkpoint will naturally allocate
the same next track, clip, pattern, asset, kit, mixer, or AIR ID. The
universal-weave tests avoid this problem with a counter shared by all virtual
peers; globally unique weave node IDs solve only the graph-node collision.

Safe staged policies are:

1. **Online v1:** the coordinator lowers create intents and allocates final
   domain IDs. Clients may optimistically display actor-scoped temporary UI
   IDs, but those never enter a durable command.
2. **Limited offline v1:** edits to existing objects may queue. Creation is
   unavailable unless the replica already holds a durable lease.
3. **Offline creation:** the coordinator leases non-overlapping ranges per
   domain, with lease identity and exhaustion persisted. Exact puts below a
   later high-water mark must be regression-tested on every domain before this
   is enabled.
4. **Possible future:** migrate project identities to globally unique typed
   values. This is a project-format migration, not a collaboration shortcut.

Packing actor bits into current u64 IDs without a format contract is rejected:
it can jump allocator high-water marks, waste the space, leak replica identity,
and still fails to define reuse and migration behavior.

## 8. Media and audio transport

### Project commands never carry PCM

The current journal deliberately records durable asset metadata/commands, not
runtime PCM. Replay hydrates PCM separately and may reject a `Present` asset
whose exact samples are unavailable. That same boundary should become the
multiplayer asset channel.

Do not place source audio, decoded PCM, waveform tiles, stems, plugin binaries,
or render caches inside Loro or weave node contents. Use a separate
content-addressed service with:

- SHA-256 or BLAKE3 over canonical bytes and an explicit hash/canonicalization
  version;
- decoded sample rate, channels, frame count, and byte/sample format;
- chunking, resumability, size limits, integrity verification, and local cache
  policy;
- access control and explicit upload consent; and
- provenance/license metadata which never implies redistribution rights.

The existing `assets::ContentFingerprint` is FNV-1a 128 and explicitly only a
duplicate/relink hint. It is not an authenticity, authorization, or remote CAS
key. Reading sources already require collision-resistant `PortableDigest`
values and should keep their default of **no source PCM transfer**. Derived
reading attachments remain explicit opt-in content refs.

For an intentionally shared project asset, acceptance should be two-phase:

1. authorize and publish/locate the strong content object;
2. ensure the coordinator can hydrate and validate it, then accept the asset
   command and advertise its content ref.

A receiving editor fetches/validates the content before applying a record that
requires present PCM, or enters an explicit catching-up state. Listen-only
participants may receive a labeled host audio stream, but that stream is not a
render proof or project state.

### Availability and paths are currently too global

`MediaAsset` currently persists absolute/project-relative locations and one
`AssetAvailability`. In multiplayer, `/music/kick.wav` and “missing on this
laptop” are replica-local facts. One participant scanning or relinking must
not make every participant's asset missing/present or expose a private path.

Before comprehensive asset collaboration, split:

- shared asset identity, decoded geometry, content ref, user-authored name,
  usages, and origin provenance; from
- replica-local resolution routes, availability, fetch progress, cache path,
  and local relink candidates.

An explicit project-bundled relative media object can be shared. An absolute
filesystem path cannot.

### Deterministic audio remains local execution

Replicas do not converge audio by exchanging floating-point buffers. They
converge the accepted command sequence and verify:

- aggregate/per-domain revisions and a canonical project-state digest;
- exact dependency content digests and runtime generations;
- engine ABI, canonical block partition, format, configuration digest, and
  performance seed; and
- the declared `DeterminismGrade`.

For `BitExact`, canonical PCM digests should match. For
`StableWithinTolerance`, the comparison recipe/tolerance must be explicit.
For `NonDeterministic`, the UI must not claim replica equality; a pinned host
render may be the shared audition artifact. A digest mismatch halts accepted
record application and requests a verified checkpoint instead of continuing
from divergent state.

## 9. Offline, reconnect, panes, and presence

An offline replica has a private branch based on its last verified accepted
head. It may render and edit that branch locally, clearly labeled as unshared.
On reconnect it sends immutable submissions with their original observed head
and causal parents:

- submissions that still pass semantic rebase are accepted one by one in
  coordinator order;
- conflicting ones become pending branches with audition/diff actions;
- unavailable or unauthorized assets block only dependent proposals;
- the replica then replays the accepted suffix or receives a verified
  checkpoint plus suffix.

Offline publication must be idempotent by `operation_id`; retries never create
duplicate edits. A client cannot claim an accepted sequence or resulting
revision.

The PIA's local/shared distinction applies unchanged:

- workspace descriptors, pane multiplicity, viewport, zoom, follow, tool,
  drafts, and inspector folding remain local;
- selections/cursors, focused `ObjectRef`, audition subject, and “follow me”
  are ephemeral presence with actor and expiry;
- semantic link groups are local unless users explicitly share a presentation
  session;
- audition is private by default;
- host transport can publish a future start epoch, play/stop, loop range, and
  accepted project revision, but local device timing remains an audio-service
  concern and transport messages never dirty the project.

This makes the application feel multiplayer without turning every pane action
into permanent shared data.

## 10. Permissions and hostile boundaries

The collaboration layer must enforce role/capability policy before decode and
application. At minimum: owner, editor, interpretation contributor,
commenter/presence, and listener. Policies may also restrict project domains,
assets, export, transport control, and plugin requirements.

ACL changes live in a separately authenticated policy log. They are not weave
metadata and cannot be granted by a project command. Unknown command records
remain round-trippable but unexecuted, matching the current codec rule.

Network input additionally needs message-size/depth limits, duplicate-ID and
digest checks, causal-parent bounds, replay protection, rate limiting, and
fuzzing. The command journal's FNV checksum detects accidents/torn writes; it
is not authentication. Accepted records need a cryptographic integrity and
authorization receipt at the collaboration boundary.

## 11. Staged implementation gates

Each gate is independently useful. None requires delaying the pane and product
convergence work in `PRODUCT_INFORMATION_ARCHITECTURE.md`.

### Gate A — collaboration-ready local authority

- Add canonical strong digests for durable command batches and validated
  project checkpoints.
- Add explicit actor/operation/causal provenance alongside journal records.
- Expose a checked command dry-run/semantic-rebase API owned by `command`.
- Prove every accepted record advances exactly one revision and reproduces the
  same canonical checkpoint from the same base.
- Keep universal-weave out of `Cargo.toml`.

### Gate B — hosted online editing, metadata only

- One coordinator sequences authenticated submissions.
- Start with existing-object edits and coordinator-allocated creates.
- Broadcast accepted records; clients verify sequence, base/result revision,
  and state digest.
- Retain conflicts as inspectable proposals with typed reasons.
- Add ephemeral presence/selection and optional host transport separately.

This is already genuinely useful multiplayer for arranging, pattern editing,
automation, mixer control, and interpretation work on media every editor has.

### Gate C — asset collaboration

- Introduce strong canonical media refs and the authorized chunked side
  channel.
- Split shared asset facts from replica-local resolution/availability.
- Gate command acceptance and replay on required content readiness.
- Test missing, delayed, denied, corrupt, and differently decoded media.

### Gate D — readings and competing interpretations

- Exchange `ReadingFile` values unchanged and preserve qualified identities.
- Add attributed proposal/diff/import surfaces; selections remain per-user.
- Apply chosen constructions only through ordinary accepted envelopes.
- This is the earliest gate where a local `IndependentWeave` branch/ancestry
  adapter is likely to earn its dependency cost.

### Gate E — offline branches and semantic merge

- Choose and implement persisted domain-ID leases or a global-ID migration.
- Add proposal ancestry, idempotent reconnect, deterministic acceptance order,
  explicit resolution envelopes, and collaborative-undo behavior.
- Property-test permutations of nonconflicting submissions and retention of
  every conflict.

### Gate F — universal-weave collaborative adapter

Adopt the Loro wrapper only after all of the following are true:

- graph contents are immutable digest refs and accepted nodes cannot be
  removed/moved/split/merged;
- shared active/bookmark state is unused or separated from personal state;
- audec owns authentication, limits, versioning, unknown retention, and
  network transport around Loro updates;
- globally unique collaboration node IDs are generated independently of all
  project and reading IDs;
- imported graph updates cannot invoke commands directly;
- two-/three-peer tests cover duplicate delivery, reordering, long
  disconnection, malicious/corrupt input, and node-ID collision; and
- merge ancestry is either represented by an audited audec immutable record
  or universal-weave has gained a suitable collaborative DAG.

Until that gate, an in-memory audec graph index is simpler and does not confuse
the near-term implementation.

## 12. Required convergence tests

The multiplayer claim is not complete until these tests pass:

1. Replaying one accepted record stream from the same checkpoint yields
   byte-equivalent project codec payloads, revisions, allocator states, render
   plan identity, and (when `BitExact`) PCM digest on every replica.
2. Every permutation of a generated set of disjoint, semantically commuting
   submissions either yields the same state or is serialized into the same
   coordinator order. Cross-domain invariant failures are retained, never
   dropped.
3. Two edits of the same put address never silently choose a winner. Both
   remain attributed and auditionable until an explicit resolution envelope.
4. Concurrent create attempts cannot collide in any arrangement, sequencer,
   automation, mixer, sample-kit, asset, binding, or AIR ID space.
5. Undo after another actor's dependent edit either applies a valid inverse or
   becomes a proposal; it never rewrites accepted history.
6. A foreign reading entity retains its complete qualified ID through
   exchange, diff, merge, import, save/open, and later reading revisions.
7. Source-fingerprint mismatch and unauthorized/no-rights audio transfer are
   refusals. Reading exchange still works graph-only.
8. Missing local media cannot change another replica's shared asset state.
9. Reconnect is idempotent across duplicate, reordered, truncated, and
   partially acknowledged update batches.
10. An unknown command, schema major, graph record, or weave node payload is
    retained where promised but never executed.
11. Pane creation, focus, viewport, and audition on one replica do not dirty
    the project or rearrange another replica's desk.
12. A Loro/weave convergence result is never considered sufficient unless the
    corresponding audec accepted-state and audio tests also pass.

## 13. Explicit refusals

- No CRDT map directly over `DawProject`, domain DTOs, mixer fields, or pane
  copies.
- No last-write-wins replacement of put-style command preconditions.
- No universal-weave `WeaveAction` as an alternate command language.
- No Loro peer ID as author identity or authorization.
- No shared weave `active` node as the audible project head.
- No deletion/move of accepted history nodes.
- No flattening reading-qualified IDs into project or weave local IDs.
- No PCM, stems, caches, private absolute paths, or plugin binaries inside the
  CRDT document.
- No claim that a converged project necessarily renders identically when its
  determinism grade or dependencies say otherwise.
- No peer-to-peer offline creation until project-ID collision handling is a
  persisted, tested contract.

## Recommendation in one sentence

Build multiplayer as **audec's validated command authority plus attributed
proposal branches and separate asset/presence channels**, then let
universal-weave power the branch/reading experience behind that seam; doing so
can become comprehensive without corrupting command authority, audio
determinism, provenance, or reading identity, while making universal-weave the
state engine today would make the implementation both less safe and more
confusing.
