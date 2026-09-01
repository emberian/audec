# The next campaign: instrument, decompiler, medium

Status: post-Cycle-10 campaign orientation, 2026-08-31. This document picks up
where [SWARM_CYCLES.md](SWARM_CYCLES.md) leaves off. It is an ordering argument,
not a frozen implementation prescription. At each brick-wave briefing, copy
the real signatures from the tree and let what the implementation teaches us
change the route without weakening the product gates.

## Where audec actually is

Cycle 10 crossed an important threshold. The repository no longer mainly
lacks nouns. It has a command-owned aggregate project, deterministic journal,
runtime media cohort, project session, persistent renderer, master render
tiles, dynamic workspace document, transactional native-layout authority,
typed product navigation, artifact and interpretation stores, executable
source programs, comparisons, readings, isolated model and plug-in protocols,
and substantial production editors. The frozen application/worker/plug-in
corpus has 1,078 passing checks.

The remaining distance is nevertheless large, because a tested substrate is
not the same thing as a musician's instrument:

- `ui.rs` is still a twelve-thousand-line compatibility root in which several
  otherwise-authoritative services meet by hand. Some successful actions still
  stop at status text, a demo fallback, or an unconnected typed consequence.
- The portable workspace, native-actuation state machine, semantic tree, and
  dynamic pane registry exist, but the running GPUI shell has not yet made all
  of them the sole path. Native movement, failure recovery, focus, scroll,
  accessibility, and lifecycle need desktop evidence, not only model proof.
- Project commands own installed domains, but compatibility locks and
  reconcile code still exist around the normal path. They make it too easy for
  a new editor to grow another local truth.
- Master-tile incremental bounce is real, but dependency precision, bus/stem
  products, plug-in latency/tails, continuous control changes, cache pressure,
  and large-session scheduling are not yet one instrument-grade loop.
- Artifact-backed promotion, explanation workbench models, aligned comparison,
  reading/query documents, reveal receipts, and reverse panes all exist. The
  live shell now retains imported analysis AIR and resolves reading audition
  and reveal through the shared renderer, transport, and semantic selection;
  some reverse edit consequences still advertise their missing adapter
  honestly.
- Symphonia and Rubato now serve explicit media seams, and CLAP runs in a
  controlled subprocess fixture. General plug-in compatibility, MIDI, local
  model execution, source separation, physical Linux, and release packaging
  remain capability work rather than completed product surfaces.

This means the next campaign should spend less time inventing parallel model
vocabularies and more time making the existing authorities unavoidable. Once
that convergence is real, broad new production and reverse features become
cheaper instead of making the application shell more fragile.

## The campaign's three simultaneous promises

Every cycle must advance all three promises, though one may be the headline.

1. **Instrument.** The shortest possible path from intent to sound: select,
   sample, play, arrange, automate, mix, save, reopen, and keep hearing changes
   without losing the loop or trusting a phantom UI state.
2. **Decompiler.** Finished audio becomes competing evidence-linked programs
   which can be rendered, edited, compared, and disproved by their residual.
   A useful hypothesis is allowed to be generative or incomplete; it is never
   silently promoted to recovered truth.
3. **Medium.** A reading, command, query, or generator term can travel without
   importing a mutable scripting runtime or flattening identity and
   provenance. People should eventually be able to exchange interpretations
   and make pull requests for hearing.

The shared laws remain unchanged: one command authority, one renderer for
playback/comparison/export, one project transport, one portable workspace
document, no view-owned truth, no in-process general-purpose scripting, and no
source or instrument identity invented from a classifier label.

## Six big steps

These are dependency-shaped product sentences. A cycle may repeat a
brick/convergence/checker wave, and adjacent cycles may overlap where their
interfaces are already stable. Completion is a user journey, not the presence
of a module with the right name.

### Cycle 11 — one coherent creative desk

The first move is convergence rather than breadth. Make the running application
feel like one program whose panes happen to show different aspects of the same
project.

The GPUI root becomes a thin `ProjectWindow`/`WorkspaceRoot` around one
`Entity<ProjectSession>`, one workspace command authority, one audio
controller, and addressed pane entities. The compatibility Workbench may
remain while slices are extracted, but no new project behavior should be added
to it directly. Product actions enter through a typed action/result bridge;
background work returns an owner, generation, receipt, reveal, and diagnostic
instead of editing GPUI state opportunistically.

This cycle connects every already-modeled terminal edge which currently stops
short: reading-result reveal and shared audition, reverse edit consequences,
explain/promote/compare completion, project actions and dirty-close lifecycle,
object creation followed by exact destination reveal, and native
float/dock/dock-back/failure recovery. Legacy domain locks become read-only
compatibility views and then disappear from the installed-project path.
Viewport, time selection, loop, cursor, playhead/resume point, and follow state
remain separate across every timeline-bearing pane.

The UI architecture should grow sideways instead of downward into `ui.rs`:
pane presenters own local controls and gesture previews; application adapters
translate their typed effects into session, workspace, render, and navigation
operations; services publish immutable snapshots back. This is a gradual
strangler boundary, not a big-bang GPUI rewrite.

**Musician gate:** open *Like a Pen*, drag a new region over an existing loop,
hear the new loop rather than an old resume point, make a beat from it, land in
the created pads and pattern, edit while it loops, hear the next coherent
render, save, quit cleanly, reopen with the same workspace, and export the
audible revision. Float an analysis pane, dock it back, and confirm that its
selection, viewport, transport, and in-flight work did not fork.

**Reverse gate:** retain a finding, open it as an explanation/comparison, move
between source/construction/residual on the shared transport, reveal its
evidence and construction objects, apply one explicit edit consequence, then
undo the whole promotion through normal project history.

### Cycle 12 — an instrument-grade forward engine

With one desk and one authority, deepen the production loop until audec is an
excellent compact electronic-music DAW rather than an analyzer with capable
editor islands.

Extend incremental bounce from a master-only cache into a dependency-aware
schedule which can publish master, bus, stem, audition, and comparison
products from the same `DawEngineSchedule`. Change sets, route topology,
instrument identity, automation targets, plug-in latency, and tail reachability
become explicit render dependencies. Loop/playhead tiles remain first;
complete cohorts replace complete cohorts; export still nulls against what was
heard. Continuous controls may have a bounded preview path, but committed sound
always converges through the authoritative render recipe—never through a
second project graph.

Make arrangement, piano roll, step sequencer, sampler, mixer, and automation
share the same editing quality: marquee and multi-object transforms, exact
numeric entry, drag/drop among material/pad/track, pattern definition versus
occurrence clarity, velocity/expression lanes, reusable automation, routing,
groups/returns, meters, and keyboard completeness. Cross-rate imports should
materialize through an explicit Rubato recipe rather than decode successfully
and then refuse durable binding. Pitch-preserving stretch can be evaluated as
an immutable offline product with latency/tail provenance.

CLAP graduates from the hostile fixture to an intentionally small real-world
compatibility matrix: discovery, quarantine, state, parameters, note ports,
latency, tails, automation, deterministic offline render, and crash recovery.
MIDI 1 input is the highest-leverage new performance surface once the event
clock contract is owned; `midir` + `wmidi` over a bounded `rtrb` ingress is the
preferred narrow implementation. Durable recording still becomes commands on
the control side. The feature-gated direct CPAL host now replaces Rodio behind
the application audio contract, consumes the same `CohortRenderer`, mixes
finite previews into that one callback, and publishes recovery diagnostics.
Hardware-calibrated input, device selection UI, latency measurement, and xrun
telemetry remain the evidence needed before it becomes the default backend.

**Musician gate:** from a fresh project, build an eight-track electronic sketch
with audio, steps, notes, a sample instrument, an effect, sends, automation,
and a tempo change; reshape it while looping without device restart; save,
reopen, and export the same frozen sound. A plug-in crash or missing MIDI
device leaves the project recoverable and visibly diagnosed.

### Cycle 13 — the universal reverse instrument

Now make decompilation the ordinary way to create production objects, not a
special demo path beside them.

One explanation workbench should accept any retained artifact or finding and
offer the same progression: evidence and alternatives, source-program preview,
atomic promotion, aligned source/construction/residual/excess, coverage, edit,
refresh, retain, and read. Aspect geometry and signal layer become global
context shared by waterfall, rhythm, HPSS, Loom, components, arrangement, and
comparison panes. Coverage guides attention but never masquerades as
correctness.

For electronic music, widen the deterministic source-program vocabulary where
the existing evidence is strongest:

- rhythmic families with exact medoids, onset ranges, microtiming, swing,
  ratchets, and competing grids;
- notes, bends, glides, vibrato, envelopes, periodic control, and measured
  modulation expressed in physical source time;
- harmonic/transient and time-frequency material scopes which can become
  cited samples, masks, or overlapping claims without being called stems;
- recurrence, stereo motion, loudness/filter motion, and texture descriptors
  retained as evidence or editable curve/program candidates;
- exact-audio citation as the explicit high-description-cost fallback.

Programs stay symbolic during search and acquire project-local samples,
instruments, patterns, curves, clips, routes, and AIR backlinks only inside one
prepared command envelope. Ranking exposes residual, excess, description cost,
assumptions, and evidence separately. The shortest expression is a prior, not
a claim about how the producer worked.

**Reverse-musician gate:** select a difficult passage from *Silent Shout*, ask
for several competing constructions, hear each against the exact source,
promote one, edit its samples/pattern/curves as music, and hear the residual
change at the next loop boundary. Undo returns the project, evidence graph, and
audible construction together. No family becomes “kick,” “voice,” or “synth”
without an attributed claim.

### Cycle 14 — opt-in model hypotheses and source separation

Only after the native reverse loop is coherent should downloaded models enter
the product. They use the existing process supervisor, verified artifact
store, inference recipes, source claims, deprojection DAG, and comparison
renderer. There is no model-specific project mutation or stems-only side UI.

The first real adapter should be small enough to debug installation,
cancellation, progress, cache leases, shape validation, and worker death. Beat
This and Basic Pitch remain good initial candidates. Broad separation follows
with a pinned, locally benchmarked HTDemucs/MLX adapter; the audited vocal
RoFormer is a specialist alternative. Any later SCNet, RoFormer, drum, or
inverse-synthesis checkpoint competes on an exact artifact hash, license and
training-provenance disposition, Mac/Linux resource envelope, chunk-boundary
behavior, reconstruction/additivity contract, and usefulness to downstream
editable programs—not on an architecture name or leaderboard score alone.

Separated audio is an evidence-bearing source claim. A joint additive model
must prove its sum contract; overlapping and generative estimates show excess
and residual and are never silently summed into “the stems.” Raw onset, pitch,
contour, mask, confidence, and embedding outputs remain available to native
search rather than being collapsed into one accepted answer.

**Model gate:** install one small local model, cancel and restart it, survive a
worker crash, reopen a cached result with identical provenance, compare its
claims against native alternatives, and promote selected events/notes/material
into editable objects. Then run one opt-in separator on real electronic music
and make its bleed, sum error, residual, runtime, and peak memory as visible as
its most flattering stem.

### Cycle 15 — a language and medium for hearing

Turn the latent languages into a stable public composition surface while
keeping the trusted kernel small.

Pattern and curve terms gain the ergonomic constructs musicians actually need
without losing canonical printing, cycle semantics, deterministic seeds, and
divergence after hand edits. Aspect expressions become reusable noun phrases;
AIR queries become provenance-carrying questions; command envelopes remain the
only verbs. A headless process protocol and CLI can let Rust, Python, notebooks,
agents, or shell tools compute and propose terms outside the GPUI process. The
kernel validates commands and content-addresses pure computations; it does not
embed Lua, Rhai, Python, or another mutable authority.

Readings graduate from durable structures to a social artifact: export and
verify without source audio, resolve against matching local material, diff
terms and claims semantically, preserve unknown sections, merge as coexisting
hypotheses, reveal every imported object, and audition derived products when
local evidence permits. A literate reading can interleave prose, query,
program, comparison, and residual without pretending the prose is universal
measurement.

**Medium gate:** send a reading of one *Silent Shout* passage to another fresh
audec installation without the recording; verify it against the recipient's
matching material, reproduce its queries and programs, audition its failures,
compare a counter-reading, and import selected alternatives as one undoable
plan without renumbering foreign identity.

### Cycle 16 — scale, retirement, and release confidence

This is not a cleanup tail. It is where the architecture disappears into
trustworthy muscle memory.

Profile long/high-rate projects, dense arrangements, cache churn, plug-in
latency/tails, multiwindow work, and repeated save/recovery. Improve bounded
parallelism and storage only from those measurements. Finish physical Linux
X11 and Wayland, portals, audio/MIDI, packaging, and multiwindow testing; keep
macOS signing and bundle behavior boring. `cargo-packager` is a sensible
release-tool candidate after locked release builds exist. Windows can follow
the same platform seams without delaying honest macOS/Linux support.

Promote stable module seams into workspace crates when the dependency graph
already points one way. A likely end state is a portable project/command/codec
crate, deterministic DSP/render crate, worker-protocol crate, and GPUI app with
small worker binaries; exact package names matter less than proving that GPUI,
Rodio/CPAL, filesystem, and process handles cannot leak downward. Do not spend
a product cycle moving files merely to make this diagram true.

Retire the fixed atlas, legacy persistence and session arrangement, normal-path
deep-diff reconciliation, duplicate editor histories, and Workbench ownership
only after their replacements pass the real corpus. Add property, fuzz,
concurrency, and performance harnesses at the untrusted and realtime seams.
Translate the existing semantic tree into native accessibility only through a
supported GPUI or contained AccessKit bridge; never add a second event loop.

**Release gate:** on fresh macOS and Linux machines, complete the forward DAW
journey and reverse-production journey, reopen after forced interruption,
exercise plug-in/model failure, navigate by keyboard and assistive technology,
and produce checksummed packages whose exported render matches the audible
cohort. The public README and screenshots then describe only paths a new user
can actually reach.

## Foundation bricks that deserve parallel ownership

The cycles above should not serialize into one giant `ui.rs` lane. The broad
fronts can advance independently and converge through small copied contracts:

| Front | Foundation | What convergence proves |
| --- | --- | --- |
| Application kernel | typed action/effect/result routing around `ProjectSession`; owner/generation/cancellation conventions | no pane, dialog, task, or preview mutates project truth or publishes stale work |
| GPUI shell | `ProjectWindow`, workspace native actuator, pane presenter/factory registry, semantic focus/action adapter | portable layout and existing entities actually drive every native window |
| Render graph | explicit schedule dependencies, scopes, tails, latency, bus products, tile-store telemetry | incremental playback, comparison, audition, and export share one recipe and null law |
| Forward desk | arrangement/pattern/sampler/mixer/automation interaction models and product-object navigation | every edit is audible, undoable, keyboard reachable, and revealable |
| Reverse desk | artifact hydration, universal workbench, source-program compiler, comparison/coverage controller | evidence becomes editable construction and an audible error term without an authority shortcut |
| Media and devices | decode/materialization, resample/stretch recipes, audio clock/device adapter, MIDI ingress | hardware and file diversity enter through bounded observations and explicit recipes |
| Extension workers | model registry/runtime, CLAP/VST3 adapters, shared artifact/PCM transport | untrusted code can add hypotheses or DSP without entering GPUI or project memory |
| Readings and language | term/query codecs, headless protocol, verification/diff/import | external computation and collaboration remain pure proposals plus validated commands |
| Checker/musician | real-material scripts, desktop automation, audio-null oracles, hostile fixtures | a reachable workflow—not merely a foundation type—earns the claim |

Brick lanes do not each owe a whole-tree build. Convergence should be a mesh,
not one exhausted coordinator: pair writers at boundaries such as
session↔pane, render↔audio host, artifact↔workbench, and worker↔source claim;
then reserve central integration for module roots, shared codecs, GPUI root,
full compilation, and the musician corpus.

## Community leverage, with narrow ownership

Community code is valuable where it removes commodity work without taking
over Audec's semantic authorities.

| Capability | Campaign stance |
| --- | --- |
| Symphonia / Rubato | Already adopted at explicit decode and resample seams. Consolidate media hydration and provenance before upgrading Symphonia or changing engine interpolation. |
| Guise / GPUI | Keep Guise as split/tab geometry and GPUI as the sole window/event-loop owner. Audec's workspace document, entity registry, and command authority own semantics and persistence. |
| Clack / CLAP | Keep process isolation. Move off the crates.io 0.1.1 host before shipping general compatibility: pin the reviewed v0.2 line or a later published equivalent, then run a real plug-in matrix. |
| `midir` + `wmidi` + `rtrb` | High-leverage MIDI 1/device-event stack after the calibrated sample-clock and bounded-overflow contracts exist. Backend handles and port indices never become durable identity. |
| `notify` | Useful as a debounced invalidation hint for media, plug-in, and model registries. A sorted re-scan remains authority after dropped/reordered/overflowed events. |
| Rayon | Consider an owned bounded pool for pure tiles and analysis frontiers after profiling. Publish sorted keyed results and never perform commands or unordered floating reductions inside parallel iterators. |
| Signalsmith Stretch | Evaluate behind an offline pitch-preserving-stretch adapter with explicit latency, flush, ratio, compiler, and output-digest provenance. |
| VST3 bindings/SDK | Second format only through the existing isolated worker protocol; recursively pin the SDK/bridge and keep every ABI type out of project files. |
| AccessKit | Translate the existing semantic tree only when a supported GPUI/native bridge exists. It must not bring a second window event loop or semantic authority. |
| Proptest / cargo-fuzz / Loom / Criterion | Adopt as development tools around codecs, journals, project commands, publication atomics, worker/plugin bytes, and render performance. Findings become small permanent regressions. |
| `cargo-packager` | Use as pinned release tooling for checksummed app/DMG and Linux packages after runtime dependencies and desktop integration are explicit. |

Do not add RFD merely because native dialogs are difficult: GPUI's dialog path
already exists and should remain the first authority unless portal parenting or
platform evidence proves an adapter necessary. Do not add an application-wide
Tokio runtime, a community DSP graph, a second window toolkit, or an in-process
plug-in/model host.

## Replanning rules

The campaign is allowed to change. Its promises are not.

- If Cycle 11 still exposes disconnected outcomes, repeat convergence before
  adding another analysis family or editor.
- If edit-while-looping cannot meet the whole-render null law, fix dependency,
  tail, or publication semantics; do not fork playback from export.
- If a model cannot preserve exact artifacts, recipes, and claim semantics,
  keep it in a laboratory worker regardless of musical quality.
- If a package split requires redesigning unstable public types, extract one
  more module seam and postpone the move.
- If the real musician corpus contradicts a green unit corpus, the product
  journey wins. Record the failure as a deterministic regression where
  possible, then update the checkpoint claim.
- If production and reverse priorities conflict, prefer the foundation that
  makes both loops shorter: command authority, media identity, render
  publication, product reveal, and aspect/signal context are usually that
  foundation.

The destination is not “LMMS plus analysis” or “a debugger with a piano roll.”
It is an instrument in which listening closely can turn into structure, and
structure can be challenged by listening again.
