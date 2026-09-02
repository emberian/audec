# DAW architecture: constructing sound as well as decompiling it

This document is the implementation contract for making audec a capable music
production environment without turning it into an LMMS clone. The target is
roughly the useful everyday surface of a pattern-oriented DAW—audio clips,
notes, patterns, step sequencing, automation, instruments, mixer routing,
recording, plugins, and export—joined to audec's distinctive loop:

```text
recording -> evidence -> editable explanation -> render -> residual
       ^                                                |
       +---------------- ordinary production -----------+
```

The important difference is not another set of panels. A detected recurrence,
a played sampler note, a manually cut audio region, and a rendered stem must be
able to meet in one arrangement and one render graph while preserving which
parts are measurements, hypotheses, and deliberate construction.

This inventory describes the audec working tree on 2026-08-31. LMMS was
inspected as an implementation reference at local revision `4e677cb`, notably
its `Song`, `Track`/`Clip`, `PatternStore`, `MidiClip`, `AutomationClip`,
`AutomatableModel`, `Mixer`, `AudioEngine`, `ProjectJournal`, `DataFile`, MIDI,
recording, and remote-plugin code. LMMS is evidence that the feature set is
tractable, not an architecture to transplant.

## Product invariants

1. **One arrangement.** Audio, notes, steps, automation, inferred events, and
   residuals share a timeline and selection model. A pattern editor is a lens
   onto a pattern, not a second song hidden inside the song.
2. **Two time domains, one canonical render position.** Audio edits may be
   sample-anchored; musical material may be beat-anchored. Both compile to
   exact integer project frames before entering the audio thread.
3. **Non-destructive until explicitly rendered.** Trim, slip, split, stretch,
   reverse, gain, fades, routing, and analysis never rewrite source assets.
4. **Every renderable claim has provenance.** AIR objects keep source spans,
   evidence, analyzer/model revision, and confidence or explicit uncertainty.
   Constructed notes and clips are marked as authored, not inferred.
5. **Render and residual are first-class.** Any selection, track, pattern,
   hypothesis, or bus can be auditioned alone, bounced, and compared with its
   complement against the source mix.
6. **The audio callback is boring.** No allocation, locks, filesystem access,
   decoding, graph construction, plugin discovery, logging, or UI calls occur
   on the realtime thread.
7. **Every persistent edit is one command.** UI gestures preview ephemerally
   and commit one atomic, invertible transaction across arrangement, AIR,
   automation, mixer, and assets.
8. **Resolution follows the view.** Waveform and spectral views query retained
   numeric pyramids/tiles for the visible range. Zooming may not scale a stale
   bitmap into blur or imply analytical detail that was never recomputed.

## Exact gap matrix

“Foundation” means code exists and has useful tests; it does not imply that the
current GPUI workbench exposes or renders it yet.

| Capability | Existing foundation | Exact gap | Target slice |
| --- | --- | --- | --- |
| Project/session | `session.rs` has typed track/lane/clip/event/cluster IDs, selection, snap, revisions, and undo/redo. `project.rs` maps arrangement identities to AIR. | `ProjectDocument` does not own tempo, patterns, instruments, mixer, automation, asset registry, or a unified history. Session commands only mutate `Arrangement`. | 0–1 |
| Time and transport | Signed sample positions, half-open ranges, exact loop wrapping, atomic frame transport in `audio.rs`, independent `TimelineViewport`. | No beat time, tempo/time-signature map, punch range, metronome/count-in, or authoritative bridge between session and audio transports. | 0–1 |
| Audio clips | `Clip` has a timeline range, source start, gain/mute/lock. Retained PCM and waveform pyramid already exist. | No asset ID/source end, trim/slip/split/duplicate commands, loop mode, stretch/pitch/reverse, fades, clip envelopes, crossfades, channels, takes, or clip renderer. | 1 |
| Arrangement UI | Shared playhead and viewport primitives exist; workbench and lenses render source-derived lanes. | No canonical multitrack canvas, direct clip manipulation, marquee/range selection, track height/vertical scroll, edit cursor, tool modes, or overview/minimap. | 1 |
| Detail rendering | Waveform pyramid and retained numeric spectrum exist. `lens.rs` separates analysis recipes from view parameters. | Every plot must choose LOD from visible samples and physical pixels. Log-frequency zoom currently can leave a low-resolution raster enlarged and blurred. No cancellable spectral tile cache keyed by recipe/range/LOD. | 1, then 5 |
| Reusable patterns | Loom clusters/events already express one reusable sound family and occurrences. | No general pattern definition/instance split, linked editing, repeat range, per-instance overrides, make-unique, nested prohibition, or pattern library. | 2 |
| Piano roll and notes | AIR supports multiple pitch trajectories, unvoiced spans, pitch transforms, and modulation. | No authored `NoteEvent`, note pattern, beat-relative duration, velocity/release velocity, per-note expression, scale/grid editing, quantize/humanize, chord operations, or note renderer. AIR pitch observations must not be conflated with MIDI notes. | 3 |
| Step/drum sequencing | Rhythm, onset clusters, NMFD/recurrence work, and Loom templates can propose hit families. | No step pattern, lanes/pads, probability, ratchets, microtiming, gate, per-step pitch/gain/pan, swing, sample mapping, or record-to-steps. | 2 |
| Automation/modulation | AIR has typed parameters, curves, interpolation, binding modes, LFO/envelope/object-feature modulation. Mixer exposes normalized plugin parameters. | AIR automation is object-relative evidence/construction, not yet a project-wide control system. No universal parameter address, beat/sample lanes, touch/latch/write, control-rate compiler, UI editor, or sample-offset RT events. Mixer and AIR parameter ID spaces are unrelated. | 4 |
| Mixer model | `mixer.rs` has typed buses, inserts, sends, mute/solo, gain/pan, cycle validation, plugin descriptors, commands, and deterministic latency plans. | Not owned by `ProjectDocument`, not rendered, no track-to-bus assignment, meters, pre/post metering, sidechain port semantics, PFL, channel layouts, automation binding, or active DSP graph. | 4 |
| Audio graph | `ProjectRenderer`, exact transport source, separate audition bus, and offline `ProjectRenderSource` seam exist. | Current production path is still essentially retained PCM. No compiled clip/instrument/mixer graph, RT graph swap, scheduled note/control events, preallocated buffer arena, delay nodes, graph diagnostics, or unified online/offline engine. | 1, 3–4 |
| Samples/instruments | Retained `ProjectAudio`, source spans, Loom excerpts, audition, and source-backed reconstruction exist. | No content-addressed asset library, native sampler, multisample/key mapping, envelopes/filter, one-shot/loop modes, choke groups, basic synth, instrument instance/state, or preset format. | 2–3 |
| MIDI | Note/pitch concepts and transport recording mode exist. | No device backend, port registry, timestamped input queue, clock/MMC policy, mappings/learn, note recording, overdub, sustain handling, CC/pitch bend/channel pressure, MPE, MIDI-file import/export, or latency calibration. | 3, 6 |
| Audio recording | Transport has a `Recording` mode. | No input device/configuration, monitor path, lock-free capture FIFO, disk writer, latency compensation, punch/count-in, clip finalization, take lanes, waveform growth, or crash-recoverable temporary media. | 6 |
| Freeze/bounce/export | `render.rs` has deterministic, cancellable offline rendering, WAV 16/24/float encoding, gain/dither, progress, and residual metrics. | Source is not yet an arrangement graph. No mix/stems/selected-bus export, tail policy, sample-rate conversion, FLAC export, freeze provenance, replace/unfreeze, consolidation, or batch render. | 4, 7 |
| Plugin hosting | Mixer has opaque plugin descriptors/state, parameters, latency, bypass/wet, and ordered inserts. A local Clack checkout has already been audited. | No scan database, CLAP host, audio/note/parameter port negotiation, process isolation policy, editor window, state restore, crash quarantine, latency-change recompilation, or missing-plugin placeholder. | 7 |
| Browser/assets | Native file open and material reference persistence exist. | No searchable browser, favorites/tags, sample preview bus integration, waveform/metadata cache, drag/drop, project asset registry, missing-file relink, duplicate-by-hash handling, or presets/templates. | 2, 7 |
| Undo/commands | `SessionCommand` and `MixerCommand` are deterministic and reversible. Session history tracks dirty state. | Separate histories cannot make an atomic “split clip and create fade” or “accept hypothesis and route it.” Closure-based edit helpers are not serializable. No gesture coalescing, command registry, or recovery journal. | 0 |
| Persistence | `persistence.rs` provides a versioned deterministic manifest, atomic replacement, linked material, workspaces/artifacts, and unknown-record preservation. `workspace.rs` round-trips dock/float layout. | Manifest does not serialize the actual session, AIR, tempo, patterns, instruments, automation, mixer, undo checkpoint, plugin state, or asset metadata. IDs lack an explicit serialized allocation contract. No autosave/recovery or migrations for model records. | 0, then every slice |
| Workspace | Guise-backed split/tab layout model, stable built-in IDs, floating placements, dock-back, and snapshots exist. | DAW editors/browser/mixer need dynamic instances, saved per-view state and link groups. Workspace placement must never own project truth. | 1–4 |
| Reverse-DAW bridge | AIR, project identity map, rhythm/NMF/NMFD/HPSS/Loom, evidence, alternatives, render/residual metrics are unusually strong foundations. | An accepted hypothesis does not yet instantiate an ordinary sampler/pattern/note/automation graph. Manual production edits do not yet retain AIR backlinks. There is no continuous explained/residual monitor bus. | 2, 5, 8 |

## Target project model

`ProjectDocument` becomes the only persistent root. Editors retain typed IDs
and dispatch commands; they do not own model fragments.

```rust
pub struct ProjectDocument {
    pub meta: ProjectMeta,
    pub tempo: TempoMap,
    pub arrangement: Arrangement,
    pub patterns: PatternLibrary,
    pub instruments: InstrumentRack,
    pub automation: AutomationGraph,
    pub mixer: MixerGraph,
    pub assets: AssetRegistry,
    pub air: AuditoryIr,
    pub identities: ProjectIdentityMap,
    pub workspaces: WorkspaceSet,
    pub limitations: Vec<EpistemicLimitation>,
    pub revision: ProjectRevision,
}
```

The present `Session` remains useful as an interaction façade, but its
persistent `Arrangement` and history move under the document. Its transport,
selection, snap configuration, active editor, audition state, and viewports
remain ephemeral `ProjectSession` state. There must not be two editable copies
of the arrangement.

### IDs and authored/inferred identity

Use project-stable typed IDs allocated from serialized monotonic counters. Do
not derive identity from vector indexes or names. IDs are never reused within
a project, including after undo. Add `serde` support and preserve unknown enum
variants through versioned opaque payloads at persistence boundaries.

The identity map should grow typed edges rather than collapsing domains:

```rust
pub enum SemanticLink {
    ClipExplainsObject { clip: ClipId, object: ObjectId },
    NotesApproximatePitch { pattern: PatternId, object: ObjectId },
    InstrumentRealizesHypothesis { instrument: InstrumentId, hypothesis: HypothesisId },
    AutomationApproximatesTransform { lane: AutomationLaneId, transform: TransformId },
    BounceDerivedFrom { asset: AssetId, revision: ProjectRevision, scope: RenderScope },
}
```

An AIR pitch trajectory is a time-varying observation, possibly polyphonic or
unvoiced. A `NoteEvent` is a discrete authored performance instruction. A
transcription command may create notes linked to the trajectory, but never
turns one into the other silently.

## Time model

Use two explicit coordinate systems:

```rust
pub struct ProjectFrame(pub i64);          // exact PCM frame, may include preroll
pub struct BeatTime(pub i64);              // signed ticks at PPQ = 960

pub enum TimeAnchor {
    Samples(ProjectFrame),
    Beats(BeatTime),
}

pub struct TempoMap {
    pub tempos: Vec<TempoPoint>,
    pub meters: Vec<MeterPoint>,
}
```

`TempoMap` owns deterministic `beat_to_frame` and `frame_to_beat` conversion at
the project sample rate. V1 supports step tempos; ramps are a later enum
variant with numerical golden tests. Store MIDI import resolution/source ticks
as metadata when necessary for exact round-trip, but schedule only compiled
project frames. Audio clips default to sample anchoring; note, step, and
automation patterns default to beat anchoring. Either can be explicitly
converted with an undoable command.

The global transport owns playhead, play/stop/record, loop, punch, count-in,
metronome, and follow policy. A lens owns only its viewport. Loop and punch
ranges are half-open, visible, draggable, and compiled into the same atomic
transport read by the audio callback.

## Arrangement and audio-region semantics

Replace the current implicit “timeline length plus source start” clip mapping
with a content payload and an explicit playback transform:

```rust
pub struct Clip {
    pub id: ClipId,
    pub lane: LaneId,
    pub placement: Placement,
    pub content: ClipContent,
    pub name: String,
    pub color: Option<ColorToken>,
    pub muted: bool,
    pub locked: bool,
}

pub enum ClipContent {
    Audio(AudioRegion),
    Pattern(PatternInstance),
    Automation(AutomationRegion),
    Analysis(AnalysisRegion),
}

pub struct AudioRegion {
    pub asset: AssetId,
    pub source: FrameRange,
    pub playback: PlaybackTransform,
    pub gain_db: f32,
    pub fades: ClipFades,
    pub channels: ChannelMapping,
}

pub struct PlaybackTransform {
    pub rate: f64,
    pub preserve_pitch: bool,
    pub pitch_semitones: f64,
    pub reverse: bool,
    pub algorithm: StretchAlgorithm,
    pub warp_markers: Vec<WarpMarker>,
}
```

The operations have exact, separately testable meanings:

- **Move** changes only `placement.start`.
- **Trim left/right** changes placement and the visible source interval while
  preserving the current source-to-timeline mapping.
- **Slip** moves the source interval without moving the placement.
- **Split** creates two regions whose adjacent renders concatenate to the
  pre-split render, including correct source mapping and complementary fades.
- **Duplicate** allocates a new clip ID but references the same immutable asset
  and pattern definition.
- **Stretch** changes playback mapping; it never masquerades as a trim.
- **Loop** repeats content inside an instance without materializing copies.
- **Fade/crossfade** is evaluated in project-frame time after source mapping;
  equal-power and linear laws are explicit.

Track kinds become capabilities rather than hard editor silos:

```rust
pub enum TrackSource {
    Audio,
    Instrument(InstrumentId),
    Pattern,
    Analysis,
    Group,
}
```

A track owns ordered lanes (main, takes, automation, annotations), a mixer bus,
and presentation state. A lane owns z-order/comp policy. Overlaps initially sum
unless an explicit comp/crossfade policy says otherwise.

## Reusable patterns and editors

Patterns are data definitions; clips are placements of definitions. This is
the central simplification that lets the arrangement, piano roll, and step
sequencer share a model.

```rust
pub struct PatternDefinition {
    pub id: PatternId,
    pub name: String,
    pub length: BeatDuration,
    pub content: PatternContent,
    pub revision: u64,
}

pub enum PatternContent {
    Notes(NotePattern),
    Steps(StepPattern),
    Events(EventPattern),
}

pub struct PatternInstance {
    pub pattern: PatternId,
    pub offset: BeatTime,
    pub repeat: RepeatMode,
    pub transpose: PitchOffset,
    pub gain_db: f32,
    pub overrides: BTreeMap<EventKey, EventOverride>,
}
```

Editing a linked definition updates all instances. **Make Unique** clones the
definition and retargets selected instances in one transaction. Per-instance
overrides are for sparse performance differences; once they become dense, the
UI offers Make Unique. V1 prohibits nested patterns to keep scheduling,
latency, serialization, and cycle behavior obvious.

### Piano roll and note ontology

```rust
pub struct NoteEvent {
    pub id: NoteId,
    pub start: BeatTime,
    pub duration: BeatDuration,
    pub pitch: NotePitch,
    pub velocity: f32,
    pub release_velocity: f32,
    pub probability: f32,
    pub channel: u8,
    pub expression: PerNoteExpression,
    pub provenance: Authorship,
}

pub struct NotePitch {
    pub midi_key: u8,
    pub cents: f32,
}

pub struct PerNoteExpression {
    pub pitch: Vec<ExpressionPoint>,
    pub pressure: Vec<ExpressionPoint>,
    pub timbre: Vec<ExpressionPoint>,
}
```

The piano-roll lens needs pointer-select/draw/erase, marquee, move/resize,
velocity and expression lanes, duplicate, split, legato, quantize with
strength, humanize with a seed, scale highlighting, chord audition, and
fold-to-used-pitches. The editor mutates a `NotePattern` through project
commands and never reaches an instrument directly.

### Step/drum sequencer

```rust
pub struct StepPattern {
    pub resolution: BeatDuration,
    pub swing: f32,
    pub lanes: Vec<StepLane>,
}

pub struct StepLane {
    pub target: TriggerTarget,       // sampler pad, note, or AIR/Loom template
    pub choke_group: Option<ChokeGroupId>,
    pub steps: BTreeMap<u32, StepEvent>,
}

pub struct StepEvent {
    pub velocity: f32,
    pub probability: f32,
    pub micro_offset: BeatDuration,
    pub gate: BeatDuration,
    pub ratchets: u8,
    pub pitch: PitchOffset,
    pub pan: f32,
}
```

The reverse-DAW payoff is immediate: onset/recurrence hypotheses can propose
lanes and steps, each linked to its evidence and accompanied by a confidence
overlay. Accepting the proposal creates an ordinary editable pattern plus a
sampler/Loom-template target. It does not erase the hypothesis. The user can
mute the reconstruction and hear the residual at every stage.

## Assets, samples, and instruments

`AssetRegistry` separates immutable media identity from its location:

```rust
pub struct AssetRecord {
    pub id: AssetId,
    pub digest: ContentDigest,
    pub kind: AssetKind,
    pub locations: Vec<AssetLocation>,
    pub media: MediaInfo,
    pub analysis: BTreeMap<RecipeDigest, ArtifactId>,
}
```

Content digests deduplicate assets and support relinking; paths remain user
intent and may be relative. The browser maintains a replaceable metadata index
outside project truth. Preview uses the existing audition bus, never pauses or
lies about the main transport, and can audition at project tempo/pitch.

Ship useful native devices before depending on third-party plugins:

1. `Sampler`: one-shot, forward/ping-pong loop, source trim, root note,
   key/velocity zones, choke groups, gain/pan, AHDSR, multimode filter, and
   optional transient-preserving pitch/time mapping.
2. `DrumRack`: pads referencing sampler presets, including Loom templates.
3. `BasicSynth`: polyphonic oscillator(s), noise, filter, envelopes, LFO,
   unison, and modulation matrix. It is intentionally understandable and
   automation-complete, not a plugin ecosystem in miniature.

```rust
pub enum InstrumentSpec {
    Sampler(SamplerSpec),
    DrumRack(DrumRackSpec),
    BasicSynth(BasicSynthSpec),
    Plugin(PluginInstanceId),
}
```

Instrument state is persistent plain data. Runtime voices and DSP allocations
live only in compiled audio nodes. A preset is a versioned instrument spec plus
referenced assets; project instances may embed or link it explicitly.

## Universal parameters, automation, and modulation

AIR's parameter model is the semantic basis, but production controls need one
project-wide address space:

```rust
pub enum ParameterAddress {
    TrackGain(TrackId),
    TrackPan(TrackId),
    SendLevel(SendId),
    ClipGain(ClipId),
    Instrument { instrument: InstrumentId, key: ParameterKey },
    Processor { processor: ProcessorId, key: PluginParameterKey },
    Air(ParameterId),
}

pub struct ParameterDescriptor {
    pub address: ParameterAddress,
    pub unit: Unit,
    pub plain_range: RangeInclusive<f64>,
    pub default: f64,
    pub mapping: ValueMapping,        // linear, log, stepped, enumerated
    pub smoothing: SmoothingPolicy,
}
```

Automation lanes bind to addresses, not object pointers. They choose a sample
or beat time domain, interpolation (hold/linear/smooth/exponential/Bezier), and
binding mode. Modulators are reusable control nodes—LFO, envelope, macro,
MIDI/OSC source, sidechain envelope follower, or AIR object feature—with an
explicit rate (audio/control/event), depth, offset, and polarity.

Compilation resolves descriptors and curves into bounded block segments and
sample-offset parameter events. Continuous parameters are smoothed in DSP;
stepped parameters are not. Automation recording supports read/touch/latch/
write and coalesces an entire gesture into one command. Plugin parameter IDs
are stable format-native keys, never scan-order indexes.

## Mixer and audio graph

Keep `MixerGraph` as the validated persistent routing description. Add:

- track/instrument/component bus assignments;
- typed audio, note-event, and sidechain ports;
- channel layouts and explicit up/downmix nodes;
- meter taps, PFL/solo-in-place policy, and record-monitor paths;
- latency-change invalidation and per-route compensation nodes;
- parameter addresses for every automatable fader, send, insert, and device.

Persistent state is compiled off-thread into a runnable graph:

```rust
pub struct GraphCompiler;

impl GraphCompiler {
    pub fn compile(
        snapshot: &ProjectSnapshot,
        assets: &AssetCache,
        plugins: &PluginRuntimeRegistry,
        config: EngineConfig,
    ) -> Result<PreparedGraph, CompileReport>;
}

pub struct ProcessContext<'a> {
    pub block_start: ProjectFrame,
    pub frames: usize,
    pub transport: TransportSnapshot,
    pub events: &'a [ScheduledEvent],
}
```

`PreparedGraph` owns a topologically sorted node program, initialized DSP
state, a preallocated audio-buffer arena, delay-compensation storage, and
bounded event queues. The control/render worker moves it through an SPSC queue.
The audio thread swaps it only at a block boundary and returns the retired
graph through another queue. Mutable DSP state is owned by the audio thread;
an `Arc` to project data is not treated as a magic realtime solution.

Small parameter/playhead changes travel as bounded `RtCommand`s with sample
offsets. Structural edits request a new compiled graph. Queue overflow is
counted and visible; note-off and all-notes-off have reserved capacity. Disk
audio is decoded ahead into pinned blocks; cache misses yield a diagnostic and
silence, never a blocking read in the callback.

Online playback and offline export instantiate the same graph compiler and
node implementations. Offline mode may use larger blocks and run faster than
realtime, but must produce the same samples within documented floating-point
tolerance for the same block partition policy.

### Realtime ownership table

| Thread/domain | Owns | May communicate by | Forbidden work |
| --- | --- | --- | --- |
| GPUI | editors, selection, commands, view state | project command channel, snapshots | DSP, decode, analysis, plugin scan |
| Project/control | authoritative document, undo, validation, graph compile requests | immutable snapshots, bounded queues | UI calls, audio-device callback |
| Analysis workers | spectral tiles, rhythm, separation, AIR proposals | cancellable jobs, content-addressed artifacts | mutating project without a command |
| Decode/cache workers | media decode and pinned active blocks | cache requests/results | blocking the RT thread |
| Audio RT | active prepared graph, DSP state, atomic transport | SPSC commands/events/meters | allocation, mutexes, I/O, logging, graph construction |
| Plugin scanner | plugin discovery/validation | versioned scan database | loading unknown code into the GUI process |
| Optional plugin worker | sandboxed plugin DSP/UI bridge | shared-memory audio/event rings | authority over project files |
| Offline renderer | private prepared graph and encoder | progress/cancel channel | sharing mutable RT DSP state |

## MIDI and recording

MIDI backends normalize platform packets into timestamped events at the device
edge. Core types cover note on/off, poly/channel pressure, CC, program change,
pitch bend, transport/clock, SysEx references, and MIDI 2.0/MPE identity without
forcing every instrument to implement all of them.

```rust
pub struct TimedMidiEvent {
    pub host_time: HostTime,
    pub source: MidiPortId,
    pub message: MidiMessage,
}
```

An input calibrator maps host time to project frames using device/input latency
and the current audio callback clock. Recording writes a temporary performance
stream first, then quantizes or converts it to notes only on commit. Raw timing
is retained so quantization is reversible. Overdub, replace, and loop-take
modes are explicit.

Audio recording uses a preallocated input FIFO feeding a disk-writer worker.
It writes a recoverable temporary WAV/RF64 plus a tiny sidecar containing
project, track, start frame, channel format, and finalized length. Stop/punch
creates an asset and clip transaction after the writer flushes. Input and
plugin latency are compensated in clip placement, with the original capture
offset retained. Monitoring has off/software/hardware modes; audec never
silently double-monitors.

## Bounce, freeze, and export

Extend the existing deterministic renderer around scopes rather than separate
code paths:

```rust
pub enum RenderScope {
    Master,
    Buses(Vec<BusId>),
    Tracks(Vec<TrackId>),
    Selection,
    Clip(ClipId),
    Pattern(PatternId),
    Explanation(HypothesisSetId),
    Residual { reference: AssetId, explained: Vec<ObjectId> },
}

pub struct TailPolicy {
    pub preroll: u64,
    pub end: TailEnd, // Exact, FixedFrames, or UntilBelow with hard maximum
}
```

Export supports master WAV first, then per-track/bus stems, FLAC, sample-rate
conversion, metadata, loop range, selection, normalization/true-peak policy,
and batch naming. Dither is only applied at final integer encoding.

Freeze renders a selected source track through its instrument and inserts to
a content-addressed asset, records the source revision, graph digest, plugin
states, render format, and tail policy, then disables the expensive live path.
Unfreeze restores it exactly. Bounce-in-place is a new authored clip and does
not hide its derivation. Consolidate deliberately replaces several regions
with one source-backed region and is separately undoable.

## Plugin hosting

Host CLAP first behind audec-owned interfaces; use Clack if its audited API
continues to fit. Do not expose Clack types in the project model. Later VST3/AU
support is an adapter decision, not a schema rewrite.

The minimum credible host includes:

1. an out-of-process scanner with timeout, crash quarantine, architecture,
   vendor/version/features, ports, parameter keys, and content identity;
2. audio/note/event port negotiation and stable parameter addressing;
3. opaque state save/restore with missing-plugin placeholders;
4. latency and tail reporting that trigger graph recompilation;
5. rescan/restart UI, crash diagnostics, bypass, wet/dry, and editor windows;
6. deterministic offline capability flags and a realtime-safety policy.

Unknown plugin code must not load during project parsing or scanning in the GUI
process. V1 can process trusted plugins in-process after external scanning, but
the host boundary must permit a shared-memory worker. A missing or crashed
plugin remains a visible silent/bypassed node with its descriptor, state, and
automation intact.

## Browser and workspace

Add dynamic workspace item kinds for Arrangement, Piano Roll, Step Sequencer,
Pattern Editor, Mixer, Automation, Asset Browser, Inspector, AIR Graph, Render,
and any number of analysis lenses. A workspace item stores only its editor
target, viewport, local parameters, and link groups. The document owns all
musical/audio state.

The asset browser searches an asynchronously maintained index by name, path,
tag, duration, sample rate, loudness, detected tempo/key (clearly labeled), and
digest. Drag/drop creates a command; hover/space auditions on the preview bus.
Missing assets expose relink-by-digest and relink-folder operations. Favorites,
recent locations, user collections, presets, and project assets are views over
the same registry/index.

Spectral and waveform views request tiles using a key such as:

```rust
pub struct SpectralTileKey {
    pub asset: AssetId,
    pub recipe: SpectralRecipeDigest,
    pub time_level: u8,
    pub frequency_level: u8,
    pub tile_x: u32,
    pub tile_y: u32,
}
```

The renderer selects a level from visible frames/bands and physical pixels,
shows the nearest available level immediately, and schedules missing finer
tiles. Completed tiles replace coarse ones without changing coordinates. A
zoom into a log-frequency band therefore becomes sharper; it never just
bilinearly enlarges the whole-song texture. Analysis-recipe changes invalidate
by digest, while palette/range changes reuse numeric tiles.

## Commands, shortcuts, undo, and persistence

Replace editor-local mutation with one serializable command vocabulary:

```rust
pub enum ProjectCommand {
    Transaction { label: String, commands: Vec<ProjectCommand> },
    InsertClips(Vec<Clip>),
    RemoveClips(Vec<ClipId>),
    EditClips(Vec<ClipPatch>),
    EditPattern(PatternPatch),
    EditAutomation(AutomationPatch),
    EditMixer(MixerPatch),
    EditAir(AirPatch),
    RegisterAsset(AssetRecord),
    AcceptHypothesis(AcceptanceRecipe),
}

pub struct AppliedCommand {
    pub inverse: ProjectCommand,
    pub revision: ProjectRevision,
    pub impact: ChangeSet,
}
```

Commands validate against stable IDs and an expected base revision. A pointer
drag updates a transient preview overlay; mouse-up commits one patch. Text/
knob/automation gestures merge by command key and time window. Undo/redo
applies stored inverses without rewinding transport or viewport. New edits
after undo clear redo. Asset deletion is reference-counted at the project
level; cache garbage collection is outside undo.

Actions are registered independently of key bindings:

```text
audec.transport.toggle             Space
audec.transport.record             R
audec.edit.undo                    Cmd-Z
audec.edit.redo                    Cmd-Shift-Z
audec.clip.split                   Cmd-E
audec.clip.duplicate               Cmd-D
audec.clip.make_unique             Cmd-U
audec.view.zoom_to_selection       Z
audec.loop.from_selection          Cmd-L
audec.snap.toggle                  S
audec.editor.arrangement           Cmd-1
audec.editor.piano_roll            Cmd-2
audec.editor.step_sequencer        Cmd-3
audec.editor.mixer                 Cmd-4
```

Contexts (`Global`, `Arrangement`, `PianoRoll`, `StepSequencer`, `Mixer`,
`TextInput`) resolve collisions; user keymaps override defaults by action ID.
Menus, buttons, command palette, accessibility labels, and shortcuts invoke the
same action. Every action advertises enabled/checked state and a short reason
when disabled.

Persistence keeps the current versioned manifest/atomic-write strengths and
adds versioned document records for project model partitions. A practical
layout is a macOS-friendly `.audec` package directory:

```text
Song.audec/
  manifest.audec          # small, atomically replaced last
  document.json           # deterministic model snapshot, schema-versioned
  media/                  # optional consolidated assets
  artifacts/<digest>      # disposable/rebuildable analysis and freeze data
  autosave/journal        # framed commands with checksum
  autosave/checkpoint
```

Linked files remain linked unless the user consolidates. Saves write new
content-addressed records, fsync, then atomically replace the manifest.
Autosave appends checksummed serializable commands and periodically compacts to
a checkpoint; startup offers recovery only when the journal is newer than the
last successful save. Unknown top-level records and missing plugins/assets
round-trip without data loss. Migrations are pure `vN -> vN+1` functions with
golden fixtures, not scattered conditionals in live model classes.

## Reverse-production bridge

The feature that justifies building a DAW inside audec is a typed deprojection
recipe:

```rust
pub struct DeprojectionRecipe {
    pub source_revision: ProjectRevision,
    pub hypotheses: Vec<HypothesisId>,
    pub creates: Vec<Construction>,
    pub links: Vec<SemanticLink>,
    pub render_scope: RenderScope,
}

pub enum Construction {
    SamplerFromSpans { spans: Vec<SpanId>, strategy: TemplateStrategy },
    PatternFromEvents { objects: Vec<ObjectId>, grid: Option<BeatGridId> },
    NotesFromPitch { object: ObjectId, voice: usize, policy: NoteSegmentation },
    AutomationFromTransform { transform: TransformId },
    BusFromComponent { object: ObjectId },
}
```

Applying it is one project transaction. It creates normal patterns,
instruments, clips, automation, and routing plus semantic backlinks. It also
creates an explanation bus whose solo renders the current explanation and a
residual monitor `source - time_aligned(explanation)`. Alignment, gain fit,
latency, and render revision are inspectable. The system must permit multiple
competing deprojections side by side; “accepted” means chosen for editing, not
declared ontologically true.

This makes several workflows unusually direct:

- drag a recurrent onset family into a drum rack, edit its step pattern, and
  hear what attacks remain unexplained;
- segment an AIR pitch trajectory into notes, route them to a native synth,
  edit articulation/automation, and compare against the pitched residual;
- turn an inferred modulation curve into filter automation while retaining
  the measured feature and alternative rates;
- bounce a separated or reconstructed component, then use it as ordinary
  source material without losing its model/license/provenance record.

## Vertical delivery slices

Each slice ends in a usable musical loop, not a collection of disconnected
types.

### Slice 0: one document, one command path

- Move arrangement ownership under `ProjectDocument`; add tempo, mixer, assets,
  stable serialization IDs, and project-wide command/history infrastructure.
- Bridge the session transport to the exact atomic audio transport.
- Serialize/open/save an empty and imported project; add autosave journal.
- Establish action registry and shared editor contexts.

**Exit:** create/import, edit, save, reopen, undo/redo, and playhead/loop state
all refer to one authoritative model; no feature owns a shadow copy.

### Slice 1: audio arrangement that can make a song

- Multitrack canvas with pixel-aware waveform LOD, vertical/horizontal scroll,
  zoom about pointer, selection, loop/punch, snapping, and follow.
- Audio asset registry and clip renderer.
- Move, trim, slip, split, duplicate, loop, reverse, stretch, fades/crossfades,
  gain/mute/solo, and basic track buses.
- Master WAV export using the same compiled graph as playback.

**Exit:** arrange several source files into a glitch/edit piece, loop and
crossfade it, save/reopen it, and export a null-consistent WAV.

### Slice 2: patterns, sampler, and drums

- Pattern library/instances, step editor, native sampler and drum rack, browser
  preview/drag, pattern linking/make-unique.
- Convert Loom recurrence clusters into pads plus a proposed step pattern.
- Explained and residual monitor buses.

**Exit:** reconstruct and substantially rearrange the percussion of an
electronic track, replace hits, and hear the source residual continuously.

### Slice 3: notes and instruments

- Piano roll, `NoteEvent`, native synth, beat scheduling, metronome, note
  audition, quantize/humanize, per-note expression.
- Pitch-trajectory-to-note deprojection with explicit alternatives.
- MIDI file import/export before live device input.

**Exit:** author a complete pitched pattern, transcribe one candidate voice,
edit it, and render it through native instruments.

### Slice 4: automation and real mixer

- Universal parameter registry, automation lanes/editor, LFO/envelope/macros,
  track/bus mixer UI, inserts/sends, meters, sidechains, latency compensation.
- Track/bus/stem render scopes, freeze/unfreeze, tail policies.

**Exit:** automate a filter and send, route groups and sidechains, freeze a
track, reopen it, and export aligned stems that sum to the master.

### Slice 5: forensic production tools

- Numeric multiresolution spectral tile cache, masks, probes, annotations, and
  sample/frequency-accurate selection shared with the arrangement.
- Deproject transformations/modulation and render counterfactual variants.
- Make analysis workers cancellable and content-addressed.

**Exit:** zoom deeply without blur, edit a spectral/time region or inferred
transform, and audition both explanation and complement.

### Slice 6: performance and capture

- Live MIDI backend, learn/mappings, note/CC/MPE recording, automation modes.
- Audio input, monitoring, calibration, punch/count-in, recoverable capture,
  take lanes, and comping.

**Exit:** record audio and MIDI through a loop, recover an interrupted take,
and commit latency-compensated clips without blocking playback.

### Slice 7: ecosystem and delivery

- CLAP scanner/host, plugin editor windows and missing/crash states.
- Expanded browser/presets/templates, FLAC/sample-rate conversion, batch/stem
  export, consolidate and bounce in place.

**Exit:** load, automate, save, reopen, freeze, and export a CLAP instrument
and effect; reopening without the plugin loses no state.

### Slice 8: sophisticated decompilation

- NMFD/reference-guided/optional pinned-ML workers feed AIR alternatives.
- More construction targets: multi-sample instruments, note expression,
  modulation matrices, effects/routing hypotheses, structural sections.
- Comparative deprojections and residual-guided refinement.

**Exit:** a nontrivial electronic recording becomes a playable project whose
claims are editable, whose origin samples remain inspectable, and whose error
is audible rather than hidden.

## Acceptance test contract

### Model and command tests

- Moving, trimming, slipping, splitting, duplicating, stretching, reversing,
  fading, and looping clips preserve all specified invariants at negative and
  very large project positions.
- Two split regions rendered adjacently equal the unsplit region within the
  algorithm's tolerance; undo restores byte-equivalent project state.
- Linked pattern edits affect every instance; Make Unique affects only selected
  instances; no pattern cycle can be created.
- Tempo conversion is monotonic and round-trips on tempo boundaries within one
  frame. Beat-anchored clips follow tempo edits; sample-anchored clips do not.
- A transaction spanning arrangement, mixer, AIR links, and assets either
  validates completely or changes nothing. Undo/redo never reuses IDs.
- Automation interpolation, binding, parameter mapping, smoothing, and
  modulation have boundary, discontinuity, and randomized property tests.

### Audio and realtime tests

- A graph fixture covering clips, sampler voices, synth notes, inserts, sends,
  sidechain, mute/solo, and latency compensation renders deterministic golden
  blocks offline.
- Online and offline engines null within the documented tolerance across
  adversarial block sizes, loop wraps, seeks, tempo boundaries, and plugin
  latency changes.
- Stem exports sum to the master when nonlinear master processing is excluded;
  when included, the UI states why stems cannot null.
- The callback passes an allocation/lock detector and a sustained stress test
  with graph swaps, dense automation, voice stealing, cache pressure, and
  preview audition. XRuns, dropped control events, and cache misses are counted.
- Note-off/all-notes-off survives ordinary event-queue saturation. Seeking,
  stopping, looping, or recompiling cannot leave stuck voices.

### Persistence and recovery tests

- Every schema version has golden open/save fixtures. Current save is
  deterministic; unknown records, missing assets, and missing plugin state
  survive a round trip.
- Abrupt termination at each write/fsync/rename boundary leaves either the old
  valid project or a recoverable new one, never a silently truncated success.
- Autosave replay produces the same revision as uninterrupted command
  execution and rejects a corrupt/truncated final frame cleanly.
- A freeze records every input identity/revision/configuration required to
  detect staleness and unfreezes to the exact live graph.

### UI interaction tests

- Arrangement, piano roll, step sequencer, automation, mixer, and analysis
  lenses invoke the same action IDs and command path from mouse, menu, shortcut,
  and command palette.
- Horizontal navigation never changes transport; manual navigation disables
  follow; seek reveals the playhead; zoom preserves the pointer anchor.
- On a Retina display, zooming a log-frequency view requests a finer numeric
  tile. The settled image has at least one source bin per physical pixel where
  available and is not a scaled copy of the previous raster.
- Tear-off/dock-back preserves editor target, selection/link policy, viewport,
  local analysis recipe, undo continuity, and playing transport.
- Disabled actions state why; destructive-looking commands have exact undo
  labels; accessibility traversal can reach transport, tracks, clips, mixer,
  and editor toolbars.

### Reverse-DAW tests

- Every inferred pattern/note/automation object links to exact source evidence,
  analyzer/model/configuration, and an explicit uncertainty statement.
- Accepting a hypothesis creates normal production entities without deleting
  alternatives; rejecting or deleting the construction leaves AIR evidence.
- Explanation plus residual reconstructs the aligned reference within a stated
  numerical tolerance. Gain, delay, tails, and unmatched regions are visible.
- Editing one reconstructed hit/note/curve updates the explanation and residual
  fast enough for audition and never relabels a mixed component as an isolated
  instrument without new evidence.

## What not to copy from LMMS

LMMS's feature decomposition is useful, but several mature constraints should
not become audec's starting constraints:

- **Do not duplicate the song inside a pattern store.** LMMS has distinct song
  and pattern track containers and pattern-index coupling. audec should use one
  arrangement plus reusable definitions and editor lenses.
- **Do not make track subclasses the center of the data model.** LMMS divides
  behavior among instrument, sample, automation, and pattern track classes.
  audec should compose content, scheduling, device, routing, and view
  capabilities so analysis lanes can become production lanes without type
  replacement.
- **Do not mix QObject/UI ownership, raw pointers, and audio truth.** GPUI views
  dispatch typed commands to a UI-independent document. Runtime DSP is compiled
  and moved to the audio thread explicitly.
- **Do not copy the per-object journalling system.** LMMS's own header warns
  that `ProjectJournal` may be rewritten. Audec already has better beginnings
  in validated before/after commands; complete that as a project transaction
  system and recovery log.
- **Do not expose plugin/model pointers as automation identity.** Use stable
  semantic parameter addresses and format-native plugin keys. Missing devices
  must preserve automation and state.
- **Do not make XML DOM the live schema or accumulate ad-hoc upgrade methods.**
  Use plain versioned model DTOs, deterministic serialization, isolated
  migrations, unknown-record preservation, and golden fixtures.
- **Do not use a coarse musical tick as the sole clock.** Maintain exact sample
  coordinates and an explicit tempo map; compile beat events to frames.
- **Do not let global engine singletons coordinate project mutation.** Multiple
  projects, windows, offline renders, preview buses, and worker analyses need
  explicit ownership and independent lifetimes.
- **Do not promise plugin abundance before host containment.** External scan,
  crash quarantine, missing-plugin placeholders, and a realtime contract come
  before format count.
- **Do not recreate every legacy convenience before proving vertical loops.**
  A small sampler, step pattern, arrangement, mixer, and export path that can
  genuinely reconstruct and reshape one electronic track is more valuable than
  dozens of disconnected dialogs.

The architectural test remains: an ordinary DAW action should make music
quickly, while an audec action should additionally answer **what evidence led
here, what is editable, what did this render explain, and what remains?**
