# Command envelope: one serializable edit language for the aggregate project

Status: normative interface design, 2026-08-31, against `main` at `dbdda7b`.
This document is the ground truth for the ENVELOPE workstream in
[SWARM_PLAN.md](SWARM_PLAN.md) and the contract every editor and language
lane will eventually emit into. Type shapes below are normative; lanes still
copy neighbor signatures from the tree at briefing time.

The envelope replaces `LiveProject`'s reconcile-by-deep-diff with an applied
term language. One envelope is simultaneously: the aggregate undo record, the
autosave journal entry, the gesture-coalescing unit, the dirty-region source
for incremental rendering ([RENDER_TILES.md](RENDER_TILES.md)), and the wire
format a future headless `audec` accepts. It does not replace the domain
models or their validation; it routes through them.

## What already exists (and is kept)

- `daw_project::DawProject::prepare_transaction / commit_prepared / transact`:
  clone-candidate, mutate, verify touched domains, validate everything,
  publish atomically under an optimistic aggregate revision. The envelope
  **applies through this path**; it never bypasses validation.
- `sequencer::SequencerCommand` is already put-style
  (`PutPattern { before, after }`, `PutClip`, `SetTempoMap`) — inverse by
  swapping `before`/`after`. This is the model the other domains adopt.
- `mixer::MixerCommand` exists with deterministic inverses.
- `arrangement::ArrangementEditor` has transactional methods but no
  serializable command form yet; `automation`, `assets`, and `bindings` have
  none. The envelope work creates put-style command forms for them.

## Normative types

```rust
/// One user-meaningful, atomic, invertible aggregate edit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    /// Undo-menu label, e.g. "Move 3 clips", "Apply rhythm proposal 4".
    pub label: String,
    /// Aggregate revision this envelope was built against.
    pub base_revision: u64,
    /// Same-key envelopes within a coalescing window merge on the undo stack.
    pub coalesce: Option<CoalesceKey>,
    /// Ordered domain commands. Application is all-or-nothing.
    pub commands: Vec<DomainCommand>,
    /// Every ID this envelope allocates, claimed explicitly up front so
    /// journal replay is deterministic and IDs never depend on runtime state.
    pub id_claims: IdClaims,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DomainCommand {
    Arrangement(ArrangementCommand),
    Sequencer(SequencerCommand),
    Automation(AutomationCommand),
    Mixer(MixerCommand),
    Assets(AssetCommand),
    Bindings(BindingCommand),
    Air(AirCommand),
}

/// Put-style template for the domains that need a new command form.
/// `before: None` is create; `after: None` is delete; both `Some` is replace.
/// Granularity is one addressable entity (a track, a clip, a lane, a bus, a
/// registration, one binding-map entry), never a whole domain snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ArrangementCommand {
    PutTrack { before: Option<TrackRecord>, after: Option<TrackRecord> },
    PutClip { before: Option<ClipRecord>, after: Option<ClipRecord> },
    PutTrackOrder { before: Vec<TrackId>, after: Vec<TrackId> },
}
// AutomationCommand: PutLane / PutPoints (whole point-vector per lane in v1).
// AssetCommand: PutRegistration / PutUsage / PutAvailability.
// BindingCommand: PutMediaAsset / PutSequencerSample / PutPatternDefinition /
//   PutPatternPlacement / PutTrackBus / PutClipBusOverride / PutAutomationLane
//   / PutAirLink — each a single map-entry put with before/after values.
// AirCommand: PutObject / PutClaim / PutRelation (v1 minimal, put-style).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoalesceKey {
    /// Stable hash of (editor id, gesture kind, primary target id).
    pub key: u64,
}

/// Explicit allocation claims, filled by the envelope builder from the
/// project's serialized monotonic allocators before application.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IdClaims {
    pub arrangement_tracks: Vec<arrangement::TrackId>,
    pub arrangement_clips: Vec<arrangement::ClipId>,
    pub sequencer_patterns: Vec<sequencer::PatternId>,
    pub sequencer_clips: Vec<sequencer::PatternClipId>,
    pub sequencer_lanes: Vec<sequencer::StepLaneId>,
    pub sequencer_notes: Vec<sequencer::NoteId>,
    pub automation_lanes: Vec<automation::AutomationLaneId>,
    pub mixer_buses: Vec<mixer::BusId>,
    pub assets: Vec<assets::AssetId>,
    pub asset_usages: Vec<assets::AssetUsageId>,
    pub binding_aliases: Vec<u64>,
}
```

### Application result

```rust
pub struct AppliedEnvelope {
    pub envelope: CommandEnvelope,
    /// The exact inverse: commands reversed, each put swapped.
    pub inverse: CommandEnvelope,
    pub revisions: ProjectRevisions,
    /// Typed dirty regions for caches and the tile renderer.
    pub change_set: ChangeSet,
}

/// What an envelope touched, at cache-invalidation granularity. This is the
/// contract RENDER_TILES.md consumes; coarse is legal, wrong is not: a
/// change_set MUST cover every audible consequence of the envelope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChangeSet {
    pub domains: BTreeSet<ProjectDomain>,
    /// Half-open project-frame ranges whose audio may have changed, per bus.
    /// `None` range set for a bus means "the whole bus".
    pub audio: BTreeMap<mixer::BusId, Option<Vec<(i64, i64)>>>,
    /// Structural mixer/routing change: all downstream audio is dirty.
    pub routing_changed: bool,
}
```

## Semantics

1. **Atomic, validated application.** `apply(project, envelope)` derives the
   touched-domain set mechanically from `commands` (callers never declare it;
   the current `TouchedDomainMismatch` foot-gun disappears), then routes
   through `prepare_transaction`/`commit_prepared`. Any command failure or
   validation issue rejects the whole envelope with the existing typed errors.
2. **Precondition checks per command.** A put whose `before` does not match
   the current entity is a conflict error naming the entity — this is what
   makes envelopes safe to replay, merge, and eventually exchange.
3. **Inverse by construction.** The inverse envelope reverses the command
   list and swaps every `before`/`after`. `apply(apply(s, e).state, inverse)`
   restores byte-equivalent domain state. IDs are never reused after undo:
   undo of a create leaves the allocator advanced (existing project rule).
4. **Coalescing.** If the undo stack's top entry has the same `CoalesceKey`
   and arrived within the coalescing window (a count of envelopes / an
   editor-session boundary, never wall-clock in tests), the new envelope
   merges: keep the earliest `before`s, latest `after`s, recompute inverse.
   One pointer drag = one stack entry.
5. **Journal.** Applied envelopes append to the autosave journal as framed,
   checksummed JSON (project_io framing idioms). Replaying the journal over
   the last checkpoint reproduces the exact aggregate revision and
   byte-equivalent domain snapshots. `id_claims` is what makes this replay
   exact.
6. **Serialization.** Serde JSON in the codec era's style: versioned wrapper,
   unknown-variant preservation at the persistence boundary (an envelope with
   an unknown command round-trips unexecuted rather than being dropped).

## Migration plan (three landings, each shippable)

- **Phase A — envelope beside reconcile.** Add the types and
  `LiveProject::apply_envelope`. Editors keep mutating shared domains;
  reconcile stays authoritative. Every `apply_envelope` asserts (debug builds)
  that applying to a copy equals the reconcile result for the same edit.
- **Phase B — editors emit.** Sequencer view already produces
  `SequencerCommand`s: wrap. Mixer/automation views likewise. Arrangement
  editor gains a command-recording mode around its transactional methods.
  The SLICE and NOTEWIRE features emit envelopes natively.
- **Phase C — demote reconcile.** `LiveProject::snapshot` stops diffing;
  domain locks become read views; reconcile survives only as a
  debug-mode integrity checker (`debug_assert_state_consistent`).
  `sequencers_equal` is deleted.

## Acceptance contract

- Journal replay from a fresh project reproduces the aggregate revision and
  byte-equivalent domains (the ENVELOPE gate in SWARM_PLAN).
- A cross-domain envelope (clip move + mixer edit + binding put) undoes and
  redoes to byte-equivalent state; redo clears on new edit.
- A stale `base_revision` or mismatched `before` rejects without any state
  change and without consuming claimed IDs.
- Coalescing truth table: same key merges; different key, undo boundary, or
  explicit `commit_gesture` does not.
- ChangeSet coverage: for a corpus of envelopes, re-rendering only the
  ChangeSet's regions equals a full re-render byte-exactly (shared test with
  RENDER_TILES.md).
- Unknown-command round-trip through save/open loses nothing.

## Non-goals

- No merging of concurrent envelopes from different sessions (future
  readings/collaboration work builds on the precondition machinery, later).
- No fine-grained `AutomationCommand` point edits in v1 (whole point-vector
  puts per lane are acceptable; refine when profiling says so).
- No bypassing of domain validation, ever, including for replay.
