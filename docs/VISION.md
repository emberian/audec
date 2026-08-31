# audec: an auditory intermediate representation workbench

## North star

audec turns a fixed recording into an inspectable, revisable hypothesis about what happened, how it is organized, what may have produced it, and what it does to a listener.

The decompiler analogy is literal. A software decompiler does not recover privileged original source; it constructs a useful intermediate representation with address mappings and uncertainty. audec should construct an **Auditory Intermediate Representation (AIR)**:

```text
samples
  → measurements
  → events and gestures
  → patterns and pulse lattices
  → persistent streams or candidate agents
  → sections and arrangement
  → production hypotheses
  → perceptual and embodied interpretations
```

Every edge remains traceable to source samples and to the transform that proposed it. AIR is not guessed MIDI, a stem folder, or a static dashboard. It is an explanation that can be challenged, auditioned, edited, rerendered, compared, and shared.

## Ontology

- **Material**: immutable source audio, content hash, channel/timebase metadata, and derived proxies.
- **Aspect**: a persistent selection in time, frequency, channel, source, or inferred-object coordinates. Aspects are the things lenses inspect and windows tear off.
- **Field**: dense evidence such as a waveform pyramid, complex spectral tiles, CQT, onset novelty, tempogram, or mask.
- **Mark**: sparse evidence anchored to an instant, interval, track, or time-frequency region.
- **Claim**: a versioned assertion about evidence, including author, epistemic kind, confidence, workflow state, provenance, and competing alternatives.
- **Hypothesis set**: alternatives that must not be collapsed prematurely: 61/122 BPM, several section boundaries, competing source continuities, or multiple pitch tracks.
- **Entity**: a proposed event, gesture, motif, stream, agent, section, or automation curve.
- **Relation**: repeats, continues, answers, masks, causes, belongs-to, foregrounds, resembles, or entrains.
- **Transform**: a reproducible recipe with input hashes, code/model identity, parameters, resolution, latency/support, and outputs.
- **Lens**: a view-plus-interaction binding over typed AIR objects. A lens never owns analysis truth.
- **Binding**: a reproducible crossmodal mapping such as onset→collision, reverb spread→defocus, or band delay→chromatic displacement.
- **Workspace**: windows, lens placements, link groups, saved comparisons, and local viewport state.

Keep three planes visually and semantically distinct:

1. **Signal**: measured energy, phase correlation, spectral centroid, sample values.
2. **Inference**: proposed beats, pitches, event clusters, components, sources, agents, sections, or production techniques.
3. **Experience**: attributed reports such as “cold,” “mucusy,” “approaching,” or “dance-demanding.”

A spectral maximum is not automatically pitch. Spectral centroid is not universally perceived brightness. A high-similarity event cluster is not automatically a snare. UI language must preserve those distinctions.

## Signature loop

```text
listen → isolate an Aspect → form a Claim → test it perceptually
       → deproject it into editable structure → compare against evidence/residual
```

Deprojection should expose its ladder rather than jump to authoritative notes:

```text
bandwise novelty → onset candidates → recurring event families
→ periodicity candidates → beat phases → meter alternatives
→ beat-synchronous patterns → editable sequence
```

Original unquantized timing, alternative pulse interpretations, confidence, and source backlinks remain visible under every edit.

## Lens family

- **Atlas**: whole-material orientation, landmarks, regions, and ancestry navigation.
- **Waterfall microscope**: freeze, probe, trace, filter/audition, and cross semantic scales from arrangement to waveform cycles.
- **Loom**: candidate hit/source lanes and beat-synchronous sequence deprojection with source ghosts.
- **Grain field**: onset-aligned repetitions, jitter, recurrence, stutter, ratchets, and texture clouds.
- **Optics**: explicit production-chain hypotheses rendered as measurable displacement, spread, bloom, attenuation, and residual.
- **Agency braid**: competing continuity/grouping hypotheses that can split, merge, braid, and remain ambiguous.
- **Structure**: self-similarity, recurrence paths, novelty, homogeneity, fitness scapes, motifs, and hierarchical sections.
- **Stereo/vector**: port the original vectorscope’s persistence/fade behavior plus mid/side, phase, and spatial trajectories.
- **vEAR**: optional user-calibrated motion mappings for silent crossmodal audition; never assumed universal.

Any lens can be duplicated, pinned, forked for method A/B, or torn into a native window. The thing being torn off is an Aspect/lens placement, not a dashboard panel.

## View parameters are first-class

The first audec exposed FFT size/window, spectral dB bias/range, waterfall proportion, waveform shaping and zero-crossing alignment, capture period/rate/scale, vectorscope persistence/brightness, view enablement, and freely resizable native windows. The GPUI application must regain this flexibility without returning to command-line-only configuration.

Every lens owns local parameters such as:

- visible time and frequency range;
- amplitude/dB range and palette transfer;
- analysis resolution, window, hop, transform family, and channel/mid-side mode;
- smoothing, persistence, normalization, detector threshold, and rendering bindings.

Parameters that change evidence rerun a versioned Transform and fork its Claims. Parameters that only change presentation remain lens-local. No control may claim to alter analysis if it only stretches a bitmap.

## Shared analysis pipeline

```text
preserved PCM + canonical sample coordinates
  ├── waveform min/max/RMS pyramid
  ├── multiresolution complex STFT pyramid
  │     ├── log-frequency/CQT projections
  │     ├── HPSS masks and reconstruction residual
  │     ├── SuperFlux and other novelty streams
  │     └── onset-centered event fingerprints
  ├── tempograms / pulse alternatives / beat timeline
  │     └── beat-synchronous feature projector
  ├── recurrence graphs / pattern starts / structural hypotheses
  ├── NMF bases + activations
  └── optional neural stems in cancellable workers
```

The native deterministic path should cover inspectable, parameterized fundamentals. Heavy ML and source separation belong in isolated, cancellable workers with content-addressed results, signed model manifests, explicit model/license provenance, progress, and crash/OOM containment.

Recommended near-term methods:

- multiband SuperFlux plus adaptive median/MAD peak picking;
- autocorrelation and Fourier tempograms with visible half/double-time alternatives;
- dynamic-programming beat tracking and separate meter/downbeat hypotheses;
- beat-synchronous chroma, timbre, and sub-beat groove features;
- sparse mutual-kNN recurrence, diagonal path enhancement, and multiscale novelty;
- HPSS soft masks, then β-NMF bases/activations, then optional vetted neural stems;
- onset-centered spectral fingerprints for recurring hit/sample candidates.

The current prototype’s pulse/onset detector and mixed-audio event clustering are exploratory Claims, not this finished pipeline.

The current implementation now retains canonical stereo PCM in a compact waveform pyramid, preserves its numeric log-frequency field, can rerun Waterfall FFT/window/dB recipes, factors visible whole-song magnitude into explicitly non-semantic NMF component hypotheses, and performs reconstructible selected-span HPSS with audible original/sustained/transient/null comparisons. Its first construction slice, Loom, aligns recurrence-cluster occurrences, selects phase-preserving reusable excerpts, estimates editable event gains/times, overlap-adds a selected-span render, and keeps the residual audible. The onset gate now uses a local median/MAD margin so compressed modulation is less readily mistaken for a train of attacks. These are substrate and vertical slices, not completion of the broader stages below: Loom’s current templates remain fixed-length mono excerpts from the mixture rather than separated source models.

## GPUI ownership and workspace

The scalable ownership model is:

```text
WorkspaceController (app-global window/session registry)
└── ProjectSession (material, transport, analysis artifacts, selection)
    ├── LensModel (kind, root Aspect, parameters, link groups)
    └── WorkspaceWindow / LensView placements (focus, bounds, GPU/UI caches)
```

No primary window owns the session. Each native window creates fresh window-local views around shared session/lens models. Shared entities outlive placements; closing the overview must not accidentally keep or destroy transport through a strong reference cycle.

Link facets are independent:

- transport/time, selection, and annotations linked by default;
- zoom, frequency range, and lens parameters local by default;
- named link groups can opt into any facet;
- pin freezes a reference while retaining backlinks;
- fork copies parameters/claims for comparison without mutation.

`guise-ui` 1.5.3 is the leading candidate for the in-window split/tab tree: it is MIT, targets GPUI 0.2.2, and provides PaneGroup splits, tabs, tear-off events, and layout snapshots. audec should own native-window transactions, dock-back, persistence, and cross-window link semantics. Zed’s workspace is an architectural reference only; its workspace crate is GPL and must not be copied into this permissively licensed project.

## Dependency posture

- Keep RustFFT; add RealFFT for real-input efficiency and retain plans/scratch buffers.
- Move canonical decoding/metadata/seeking to Symphonia and avoid decoding the same file independently for analysis and playback.
- Evaluate Spectrograms behind audec-owned interfaces and numerical golden tests; do not make a young crate an unreplaceable data model.
- Use librosa as a pinned ISC-licensed oracle for fixtures, not a runtime dependency.
- CLAP first through Clack if hosting becomes useful; scan/quarantine plugins out of process and use realtime-safe shared-memory IPC for true DSP isolation.
- Treat pretrained model rights separately from code licenses. Do not redistribute Demucs-family weights without authoritative permission.
- Avoid linked aubio/Essentia in the permissive core because of GPL/AGPL licensing.

## Staged roadmap

1. **Honest foundation**: extract `ProjectSession`; typed Field/Aspect/Claim/Transform; cancellable analysis cache; shared transport; accurate terminology.
2. **Flexible instruments**: Guise split/tab workspace, robust native tear-off/dock-back, saved layouts, linked viewport groups, old audec parameter parity.
3. **Forensic listening**: multiresolution numeric spectral tiles, phase/channel data, freeze/probe/mask audition, executable annotations.
4. **Rhythm and recurrence**: SuperFlux, tempograms, pulse alternatives, editable beats, event families, recurrence and pattern starts.
5. **Explanatory decomposition**: HPSS/residual, interactive NMF, optional isolated neural stems, source/agent alternatives.
6. **Deprojection and construction**: Loom editing, counterfactual preview, automation/gesture hypotheses, resynthesis, then plugin routing.
7. **Phenomenotechnique**: personal perceptual lexicons, crossmodal bindings, vEAR calibration, multi-author comparative readings.

The design test is simple: every mark in every window should answer **what evidence is this, how was it derived, can I hear it, and where does it go if I deproject it?**

## Research anchors

- [Brown: Calculation of a constant Q spectral transform](https://www.ee.columbia.edu/~dpwe/papers/Brown91-cqt.pdf)
- [Schörkhuber & Klapuri: Constant-Q transform toolbox](https://zenodo.org/records/849741/files/smc_2010_020.pdf)
- [Böck & Widmer: Maximum filter vibrato suppression for onset detection](https://phenicx.upf.edu/system/files/publications/Boeck_DAFx-13.pdf)
- [Ellis: Beat tracking by dynamic programming](https://www.ee.columbia.edu/~dpwe/pubs/Ellis07-beattrack.pdf)
- [Müller: Fundamentals of Music Processing, structure analysis](https://www.audiolabs-erlangen.de/resources/MIR/FMP/C4/C4.html)
- [FitzGerald: Harmonic/percussive separation using median filtering](https://dafx.de/paper-archive/2010/DAFx10/DerryFitzGerald_DAFx10_P15.pdf)
- [Smaragdis & Brown: Non-negative matrix factorization for polyphonic transcription](https://www.ee.columbia.edu/~dpwe/e6820/papers/SmarB03-nmf.pdf)
- [Sonic Visualiser](https://sonicvisualiser.org/)
- [JAMS annotation format](https://github.com/marl/jams)
