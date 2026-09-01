# Model and algorithm strategy for electronic-music decompilation

Status: research snapshot, 2026-08-31. Discovery used Kagi Search v1; every recommendation below was checked against a primary paper, official repository, model card, or artifact host.

Electronic production is hostile to a single fixed stem ontology. A synthesizer can function as kick, bass, pad, lead, texture, or several of these over one track. Reverb, delay, distortion, chorus, sidechain compression, and master-bus processing bind several production causes into the same samples. No separator can recover the unique original project from a mastered waveform.

Audec should consequently treat every result as a **ranked, audible hypothesis**. A model output may overlap another output, be a generated recreation rather than a literal component, or describe an effect without locating its source. It is never silently promoted to an original mixer channel.

## Recommended order of integration

| Order | Capability | Candidate | Why it is first | Apple Silicon / local viability | Distribution decision |
|---|---|---|---|---|---|
| P0 | Rhythm and event evidence | Native multiband SuperFlux, tempograms, dynamic-programming beat grids, NMFD event families | Fast, inspectable, supports ranked tempo/phase alternatives and per-band events instead of a single bad beat grid | Native Rust; realtime or faster | Bundle |
| P0 | Pitch, envelope, modulation, and effects evidence | Native CQT/VQT, YIN/pYIN-style tracks, harmonic salience, analytic-signal demodulation, spectral-envelope and echo/comb analysis | These measurements retain continuous curves and uncertainty that MIDI and classifiers throw away | Native Rust; viewport and offline resolutions | Bundle |
| P1 | Beat/downbeat proposal | [Beat This!](https://github.com/CPJKU/beat_this) through the audited [pure-Rust port](https://github.com/danigb/beat-this-rs) | Strong tracker; raw frame logits; no DBN required; the Rust `rten` path removes the Python worker/runtime tax | CPU confirmed on macOS/Linux; 10.6 MB small graph or 83.2 MB full graph plus a 271 KB mel graph | User-download; MIT code and weights, but authors flag copyrighted/limited-CC training material |
| P1 | Polyphonic note proposal | [Spotify Basic Pitch 0.4.0](https://github.com/spotify/basic-pitch) | Tiny, pitch bends, raw onset/note/contour maps, and a bundled Core ML graph | Core ML is the default macOS backend; 22.05 kHz mono, two-second windows | User-download initially; Apache-2.0 code/package, training provenance review required |
| P1 | Monophonic pitch/modulation curve | [SwiftF0](https://github.com/lars76/swift-f0) | A tiny continuous F0 + confidence proposal for isolated voices, basses, and leads; retains vibrato/glide instead of quantizing immediately to MIDI | 397,987-byte ONNX model; 16 kHz CPU path; upstream reports 132 ms per five seconds | Bundle candidate after training-provenance and native-runtime goldens; MIT repository/artifact |
| P1 | Four broad stems | [HTDemucs v4 MLX conversion](https://huggingface.co/jasonvassallo/demucs-htdemucs-mlx) | 44.1 kHz stereo `drums/bass/other/vocals`; 168 MB MLX artifact; gives downstream analyzers cleaner material | Apple-native MLX; establish PyTorch↔MLX golden parity before making it default | User-download; MIT is asserted for upstream checkpoint and conversion, private extra training data remains a provenance concern |
| P1 | High-quality vocal hypothesis | [Kim Vocal 2 Mel-Band RoFormer, audited MLX conversion](https://huggingface.co/mlx-community/mel-roformer-kim-vocal-2-mlx) | Better dedicated vocal prior than a generic four-stem baseline; unusually good conversion and relicense trail | MLX, 44.1 kHz stereo, ~456 MB bf16 weights; 8 s chunks with 50% overlap | User-download; MIT checkpoint, but training corpus is undisclosed |
| P1 | Functional song sections | [All-In-One](https://github.com/mir-aidj/all-in-one) single fold | Tempo, beat/downbeat logits, 100 Hz section boundaries, labels, and embeddings; a single checkpoint is ~1.4 MB | CPU confirmed; much heavier if Audec has not already produced its four demixed inputs | User-download; MIT code/HF weights; use labels as weak claims |
| P2 | Editable nine-lane drums | [Inverse Drum Machine](https://github.com/bernardo-torres/inverse-drum-machine) | Joint onset/velocity transcription, one-shot synthesis, and separated drum tracks is closer to decompilation than a static drum stem | Tiny 2.5 MB model, PyTorch CPU should work; MPS is unvalidated | Experimental user-download; Apache-2.0 repo, narrow six-kit training distribution |
| P2 | DX7 preset retrieval | [FM-SynAPSE](https://github.com/DBraun/SynAPSE) | Topology-aware audio↔preset embeddings; returns an editable Dexed/DX7 patch candidate rather than only a tag | JAX CPU on macOS; released model is ~377 MB; do not assume JAX Metal support | Experimental user-download; MIT code/weights, documented DX7AllTheWeb training source |
| P2 | Transformed sample retrieval | [Sony SampleID](https://github.com/sony/sampleid) plus native reranking | Current official model is robust to mixing, EQ, gain, compression, and time stretch; appropriate for a user-owned sample library | PyTorch CPU; 16 kHz mono embeddings; ~805 MB checkpoint; MPS needs validation | Opt-in user-download; MIT artifact, but checkpoint used a proprietary 21k-track multitrack corpus |
| P2 | Four-stem numerical baseline | [Open-Unmix UMX-HQ](https://github.com/sigsep/open-unmix-pytorch) | Reproducible, simple, independently trained source models and generalized Wiener filtering; useful as an oracle even when weaker | CPU confirmed; ONNX conversion is tractable; do not select `umxl` by accident | UMX/UMX-HQ artifacts are MIT; `umxl` is CC BY-NC-SA 4.0 and must remain separate |
| P2 | Free-text source query | [AudioSep](https://github.com/Audio-AGI/AudioSep) | Useful for “reverb hiss,” “stuttered synth,” or other classes absent from fixed stems | PyTorch CPU/MPS experiment; 32 kHz mono and ~1.26 GB checkpoint; CUDA is documented | BYO weights pending checkpoint/training-data review; code is MIT |
| P2 | Weak sound-event labels | [PANNs Cnn14](https://github.com/qiuqiangkong/audioset_tagging_cnn) | Frame/clip AudioSet probabilities can annotate unknown regions and seed clustering without pretending to separate them | CPU works; MPS needs validation; 32 kHz mono and a moderate PyTorch checkpoint | User-download; MIT code, checkpoint and AudioSet provenance need separate review |
| P2 laboratory | Modulation-curve proposal | [LFO Modulation Extraction](https://github.com/christhetree/mod_extraction) | Wet-only model emits an editable bounded LFO curve for chorus/flanger/phaser evidence instead of only an effect label | 44.1 kHz, two-second windows; 16.6 MB Lightning checkpoint; CPU environment exists, no Apple timing | Experimental user-download; MPL-2.0, narrow 0.5–3 Hz/effect training domain |
| P2 laboratory | Multi-instrument score proposal | [MuScriptor small](https://github.com/muscriptor/muscriptor) | Current open-weight full-mix transcription frontier; emits instrument-grouped notes and drums from real productions instead of requiring a separated solo instrument | Official inference supports Linux CPU/CUDA and Apple-Silicon MPS; 16 kHz mono, five-second chunks; 103M parameters and a 411,888,600-byte safetensors artifact | BYO research use only; MIT code, gated CC BY-NC 4.0 weights with additional rights-attestation terms |
| P3 | Synth-patch proposal | [Syntheon](https://github.com/gudgud96/syntheon) Vital/Dexed and an Audec-owned optimizer | Existing Vital and Dexed demonstrations provide a seed, while Audec can optimize its own constrained synth/effect graph | CPU configured; 16 kHz, four-second, isolated/mostly monophonic assumptions; small included checkpoints | Experimental; Apache-2.0 repository, model/training provenance still needs an audit |
| P3 laboratory | Mix-style / FX-chain proposal | [StemFX](https://github.com/barry-mir/stemfx) | Produces ordered per-stem effect/parameter chains in a machine-readable 83-effect vocabulary; useful for explaining the difference between an Audec construction and a reference mix | 44.1 kHz four-stem, ten-second inputs; 115 MB model plus a 214 MB SCNet separator; CPU path exists, Apple MPS is unvalidated | Opt-in research adapter; MIT code/checkpoint, but `.pt` execution, FMA-derived training provenance, and effect-runtime determinism require review |
| P3 | Prompted separation | [GuideSep](https://github.com/YutongWen/GuideSep) | Painted TF mask plus hummed/played guide closely matches Audec interaction | CUDA-oriented diffusion inference; 1.08 GB; not a pleasant Mac default | BYO weights; repository/checkpoint say MIT, but training dependencies and generalization require review |
| Remote only | Open multimodal prompting | [SAM-Audio](https://github.com/facebookresearch/sam-audio) | Text, temporal, visual, and multimodal prompts are uniquely flexible | Large checkpoint is 14.86 GB and documented runtime is high-end/CUDA-oriented | Never bundle; custom SAM license and gated operational cost |

This order intentionally does not equate “newest” with “first.” The native lattice, Basic Pitch, Beat This!, and small MLX separators create a responsive local tool. Large promptable systems can then add alternate Claims without becoming a latency or licensing dependency.

BS-RoFormer, Mel-Band RoFormer, and SCNet-family systems are the current quality frontier for many specialist separation tasks, but an architecture's MIT training code does not license every community checkpoint made with it. Audec should register models checkpoint-by-checkpoint. Community DrumSep, MDX23C, and 6/10/17/53-stem bundles remain bring-your-own experiments until each exact weight file has an authoritative license, disclosed training provenance, immutable hash, and Mac benchmark. Kim Vocal 2 is prioritized because its particular artifact has an unusually concrete relicense and conversion record.

## Next executable slice: one inspectable claim graph

The next ML milestone is not a model browser. It is a complete, cancellable path from one loaded song to editable objects and back to an audible comparison. Implement it in this order:

1. **Use the landed claim substrate.** `ModelClaimBundle`, per-artifact wire descriptors, the authenticated model store, and `ModelTaskService` now identify the parent material or derived stem, exact sample span, `ModelManifest::canonical_hash()`, worker cache key, raw artifact hashes, model-authored label, confidence/calibration kind, time base, and per-artifact additivity. Adapter work must publish through this path and convert claims to `AnalysisProvenance` locators; never copy only the friendly stem name into reconstruction.
2. **Ship one four-stem worker.** Implement pinned `htdemucs-mlx-fp32` with the artifact below. Stage four immutable 44.1 kHz stereo outputs plus chunk recipe, peak memory, per-boundary discontinuity metrics, measured `sum(stems)-input`, and a separately derived residual. Publish all four atomically through `StagedArtifactSet`. These are overlapping source Claims unless Audec's own golden test establishes the declared mixture-consistency bound.
3. **Fan analysis out per claim.** Run the native rhythm/pitch/modulation lattice over the mixture and all four stems. Run `beat-this-small0-1.1.0` on the mixture and drum Claim; preserve logits before decoded events. Run `basic-pitch-coreml-0.4.0` on bass/other/vocal Claims and selected regions, preserving onset, note, and contour maps before MIDI. Fuse evidence by source sample span; do not average confidence numbers from unrelated models.
4. **Turn drums into editable, uncertain lanes.** Feed the drum Claim to the pinned Inverse Drum Machine adapter. Publish nine onset/velocity lanes, synthesized one-shots, Wiener-masked waveforms, and manual-correction affordances as distinct sidecars. Match its events to native anonymous `EventFamily` objects by onset and spectral recurrence. A class such as `kick` becomes a model label on an anonymous family, not its permanent identity.
5. **Compile and compare.** Extend reconstruction to emit sample clips/triggers, notes with bend curves, modulation/control candidates, the original source layer, and residual coverage from the claim bundles. Compile through the aggregate DAW engine and expose instantaneous original/reconstruction/residual audition. Acceptance is sample-aligned playback after edits, undo/redo, save/reopen with intact evidence hashes, and no unexplained audio silently discarded.
6. **Add retrieval only after local editing works.** Index a user-authorized sample library with native constellation hashes first, then pinned Sony SampleID embeddings and local reranking. FM-SynAPSE follows for isolated, pitched DX7-like Claims. Both return ranked candidates with audible A/B renders; neither auto-replaces a source.

The first release gate is stages 1–5 on an offline Apple Silicon machine after model installation. A useful product checkpoint is: select a questionable drum hit, inspect the HTDemucs and Inverse Drum Machine premises, move or relabel its trigger, hear the reconstructed mix immediately, and hear exactly what remains in the residual. That closes the loop that makes the feature decompilation rather than analysis-only visualization.

### Adapter/runtime lock for that slice

| Adapter | Immutable source and artifacts | Local runtime | License/provenance disposition |
|---|---|---|---|
| HTDemucs MLX fp32 | `jasonvassallo/demucs-htdemucs-mlx@9a32d8a73da0d6182a8a33bda927b7ea46930e44`; `model.safetensors` SHA-256 `50d904834f50980e1065f8727805211eed9ac94eb74ff8d0bd60211a4349b2f0` | `mlx-audio==0.4.3`, Apple Silicon, 44.1 kHz stereo; benchmark chunk/overlap and memory before choosing defaults | Upstream checkpoint MIT; conversion code Apache-2.0; private extra Demucs training data means opt-in download and `RequiresReview` |
| Beat This! RTen small | `danigb/beat-this-rs@089b509247e6fdcec666511c0dcf0d5f39c21e73`; mel graph SHA-256 `fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9`; small graph SHA-256 `a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f` | opt-in pure-Rust `rten` worker, 22.05 kHz mono; raw logits retained; see [`BEAT_THIS_RTEN.md`](BEAT_THIS_RTEN.md) | MIT code/graphs; training set contains copyrighted and limited-CC material, so user-install and `RequiresReview` |
| Basic Pitch Core ML | `spotify/basic-pitch@fa5997af0a8210982619003269994a1be25eddf3`; PyPI `basic-pitch==0.4.0`; Core ML artifact SHA-256 `691a6b63c7ddcdde0ee131ff3986dcb1250df47cd738612efde966ba9b4c99cd` | Core ML on macOS, 22.05 kHz mono, two-second windows; retain raw maps | Apache-2.0 code/package; checkpoint training disclosure still requires review |
| Inverse Drum Machine | `bernardo-torres/inverse-drum-machine@456656868538205ef756912c7cf5b0fd936de8af`; checkpoint SHA-256 `5856a9bee7c6d503842795756d238dc8470f6f3e010e9e4f33ede0362850cb4c` | pinned Python/torch CPU worker, 44.1 kHz mono; model is about 2.5 MB; MPS unvalidated | Apache-2.0 repository; narrow six-kit training distribution must be shown in UI |
| Sony SampleID | `sony/sampleid@e212b0b9974d52d334cc793ef1b8cabaf251e5f8`; Zenodo record `17413869` | pinned Python/torch CPU worker, 16 kHz mono; Audec supplies and records overlapping chunk windows | MIT code; checkpoint used a proprietary 21k-track multitrack corpus, so opt-in and `RequiresReview`; Zenodo publishes only MD5, compute and pin SHA-256 before adapter implementation |
| FM-SynAPSE held-out | `DBraun/SynAPSE@e58aa150e41085e6f6b09f31c821b835792207c4`; HF revision `4b0c2d08265994670eb890efd9b251c4171cc525`; weights SHA-256 `ec976c84b66396ecfc553f4693f73549e73816a50929ea8d2e4f8eced4fe6554` | JAX CPU, 44.1 kHz, four-second isolated-note query; no JAX Metal assumption | MIT code/weights; documented DX7AllTheWeb source, opt-in experimental adapter |

The broader `mlx-community/demucs-mlx@d4519e24ddc2dd4a11d56a193092433d852c3961` collection is a promising Swift/MLX follow-up: it exposes all eight upstream Demucs models and a reproducible converter. Do not silently switch the first adapter to it. Promote it only after Audec pins its config/converter hashes and demonstrates numerical and chunk-boundary parity against the fp32 lock above. NeuralSampleID is likewise a watch item rather than the default sample index: its official stack currently assumes DGL/FAISS-GPU and its code is GPL-3.0, while Sony SampleID has the simpler CPU inference surface and stronger published retrieval results.

## The native deprojection lattice

ML should sit on top of a strong deterministic evidence layer, not replace it.

### Rhythm, patterns, and instrument-like event families

1. Compute multiresolution, multiband novelty curves: complex-domain onset, SuperFlux maximum-filtered spectral flux, energy derivative, phase deviation, and low-frequency pitch-drop novelty.
2. Preserve separate low/sub, low-mid, high-mid, and noise-band evidence. A kick, bass transient, clap, and hat need not fight for one onset list.
3. Estimate autocorrelation and Fourier tempograms over several window sizes. Keep ranked BPM candidates, including half/double/triplet relationships.
4. Decode several beat/downbeat phase paths with dynamic programming or a particle/Viterbi lattice. Beat This! logits become another observation channel, not the sole truth.
5. Run NMFD with short spectrotemporal templates, then cluster activations by spectral shape, decay, pitch trajectory, stereo position, and recurrence. These are anonymous `EventFamily` Claims until a person or classifier labels them.
6. Discover repeating sequences with diagonal recurrence/local-alignment paths over event tokens. Store pattern starts, transformations, omissions, and confidence instead of flattening everything into a single bar grid.
7. Let every beat, onset, or pattern link back to exact source sample ranges and the evidence channels that caused it.

This is the right answer to electronic kicks that pitch-dive into bass, stutters closer than a conventional onset refractory period, and syncopation that a whole-mix beat model normalizes away.

### Pitch and note evidence

Use CQT/VQT peaks, harmonic summation, YIN/CMNDF tracks, reassigned-spectrogram ridges, and probabilistic continuity. Preserve:

- multiple simultaneous pitch candidates and per-frame confidence;
- unvoiced/noise probability rather than forced notes;
- bends, vibrato rate/depth, glide curves, octave ambiguity, and gaps;
- tuning offset and inharmonicity;
- linkage between a note hypothesis, its spectral ridges, and any separated source Claim.

Basic Pitch is best applied to an isolated or selected Claim. Its own documentation says it works best on one instrument at a time. On the full electronic mix it is a multipitch proposal, not an instrument-aware score. YourMT3+ is worth monitoring for multitrack transcription, but its current official surfaces are a research code/Space stack of roughly 2.8 GB with uneven source/checkpoint licensing and substantially greater runtime. It should not block the lighter path.

MuScriptor materially improves the full-mix option, but it does not replace
Basic Pitch. Its smallest official safetensors model is 411,888,600 bytes and
the upstream inference path explicitly supports CPU, CUDA, and Apple MPS. It
decodes five-second, 16 kHz mono windows into semitone MIDI events and one of
36 grouped instrument classes. It does not retain pitch bends, expressive
velocity, frame probability maps, or a source-separated identity. In
electronic music, a General-MIDI-like instrument label is especially likely to
describe timbral resemblance rather than production source. Preserve its raw
token stream and instrument conditioning as a competing Claim beside Basic
Pitch contours and native pitch ridges. The gated CC BY-NC 4.0 weights and
additional input-rights attestation make it a laboratory/BYO adapter, not a
bundled production dependency.

### Envelope, modulation, filter, and effect evidence

Estimate continuous control signals before asking a model for effect names:

- subband Hilbert envelopes and instantaneous frequency for AM/FM candidates;
- modulation spectra and envelope autocorrelation for tremolo, vibrato, periodic gating, and ratchets;
- LPC/cepstral or true-envelope tracks for time-varying formant/filter-cutoff candidates;
- spectral-centroid/rolloff/tilt trajectories with uncertainty, not a hard “low-pass” label;
- comb-notch spacing and its motion for flanger/chorus delay trajectories;
- cepstral/autocorrelation peaks for delay time and feedback candidates;
- Schroeder-style decay fits in transient-free tails for banded T60/reverb estimates;
- interchannel phase, mid/side energy, coherence, and lag for widening/panning Claims;
- envelope lag/coherence between a likely kick family and other bands for sidechain-pump Claims;
- crest factor, short/long loudness, and input/output-like envelope models for compressor hypotheses.

Blind inference from a mastered mix is non-identifiable. A low-pass sweep, oscillator harmonic change, EQ automation, and source crossfade can draw similar spectra. Saturation and bus compression destroy linear additivity. The output must therefore be a ranked `ControlCurve`/`EffectHypothesis` with supporting evidence and a resynthesized audition, never “the recovered cutoff knob.”

For an isolated candidate source, fit a deliberately small differentiable or derivative-free render graph:

```text
oscillator/sample -> pitch curve -> amp envelope -> multimode filter
                  -> waveshaper -> chorus/flanger -> delay -> reverb -> pan/gain
```

Start with spectral initialization, then CMA-ES or quality-diversity search over bounded, musically meaningful parameters and automation knots. Compare multiscale STFT magnitude, log-mel, transient-envelope, pitch-ridge, modulation-spectrum, stereo, and loudness losses. Keep a Pareto set of different patches whose rendered audio is perceptually close. [Instrumental](https://github.com/philippbogdan/instrumental) demonstrates this approach with a 28-parameter subtractive synth and reports fast MPS batching, but it currently has no declared repository license and should be treated as research inspiration rather than vendored code.

FM-SynAPSE is a particularly good precursor: query a local Dexed preset gallery, take the top candidates, then optimize their parameters while respecting the six-operator routing topology. Its published held-out result is 52.2% recall@1 and 88.5% recall@10 over 4,096 presets; that is strong retrieval, not proof that the original preset is uniquely recoverable.

### Structure and sound-event segmentation

Run boundary detection on bar-synchronous feature stacks: MFCC/timbre, chroma, CQT, loudness, novelty, event-family histograms, source-Claim embeddings, and recurrence. Fuse local novelty peaks with repeated-block alignment. All-In-One can add functional boundaries and labels, but `verse/chorus/bridge` is often the wrong ontology for electronic music; Audec should expose anonymous A/B/C regions and transformations first.

PANNs or PaSST/OpenMIC can attach weak tags such as drums, synthesizer, or guitar to regions. AudioSet/OpenMIC classes are not source separation and do not justify creating mixer lanes. Frame/clip tags belong on `LabelClaim` objects with their model vocabulary and temporal resolution.

## Source separation contracts

Every separator adapter must declare which mathematical contract it offers:

- **Additive target plus residual:** publish the model target and derive `residual = normalized_input - target`. Kim Vocal 2 and independent AudioSep queries can use this contract. Separately queried targets may still overlap one another.
- **Joint additive stems:** measure the sum error after the model's canonical Wiener/mixture-consistency stage. Do not claim `LinearSum` merely because the labels look disjoint.
- **Overlapping estimates:** independent one-vs-rest models, open-vocabulary queries, and many drum specialists can explain the same energy. Never sum them as a mixer reconstruction.
- **Generative:** diffusion separators and analysis-by-synthesis recreate a plausible source. Keep the generated audio visually and semantically distinct from literal masked mixture material.

For HTDemucs and Mel-RoFormer, retain raw outputs, exact resampling and overlap-add recipe, a derived residual, sum error, boundary artifact metrics, and bleed probes. A fixed-stem label is the model author's vocabulary, not Audec's assertion that the original producer had that track.

## Drum decompilation

The Inverse Drum Machine checkpoint is unusually aligned with Audec. It operates at 44.1 kHz mono and predicts nine classes:

```text
crash, ride, closed-hat, open-hat, kick, snare,
high-floor-tom, high/mid-tom, low/mid-tom
```

It provides onset strengths, velocities, track gains, a kit embedding, synthesized one-shots, and Wiener-masked audio. Its official checkpoint is only 2,544,694 bytes. The authors explicitly warn that performance degrades far outside the six training kits and expose manual onset overrides; Audec should make that correction workflow a primary UI, not hide it.

The August 2026 [Separate-and-Detect](https://arxiv.org/abs/2608.01093)
release jointly generates five editable drum stems and derives events. Official
code and two 5.4 GB MIT-tagged checkpoints now exist, but the recipe also needs
unlocked VAE/vocoder artifacts, runs at roughly 2.5× realtime on an RTX 6000
Ada, and emits 16 kHz generative audio. Keep it a CUDA/remote laboratory Claim
until every auxiliary artifact is licensed and hashed; never describe its
outputs as additive recovered stems.

## Sample matching

Audec should support two complementary indices over user-authorized material:

1. **Exact/near-exact local fingerprint:** spectral-peak constellation hashes with offset voting. This is tiny and precise under encoding/noise, and can return exact source time offsets.
2. **Transformed-sample retrieval:** Sony SampleID embeddings over overlapping windows, approximate-nearest-neighbor search, then native local alignment/reranking over VQT/chroma/landmark sequences. The official API accepts 16 kHz mono and time-averages each supplied chunk, so Audec—not the model—must choose and retain the chunk grid.

Never send a private sample library to a remote service by default. A retrieval result is a candidate with similarity, aligned source/query spans, tempo/pitch transform estimates, and false-positive calibration. It is not an authorship claim.

## Concrete schema-v1 worker manifests

`src/model_worker.rs` is the authority. The following are exact lock values for initial adapters. `h("…")` means `ContentHash::from_str("…")`; fields use the Rust enum names. They are construction recipes, not floating “latest” aliases.

### `kim-vocal-2-mlx-bf16`

```text
schema_version: 1
model_id: "kim-vocal-2-mlx-bf16"
architecture: { family: "mel-band-roformer", version: "kim-vocal-2@ac9b0614ab3cd7f77219e18ba494dfd93956c348" }
revision: Release {
  version: "hf-64cbfcb004e39430e5f584552c05949440ec39ce",
  source_hash: h("312c38e5b698f8dfaa4d6064e8f79010744825828917871a9d22673a43eb7fe5")
}
artifacts: {
  weights_sha256: h("312c38e5b698f8dfaa4d6064e8f79010744825828917871a9d22673a43eb7fe5"),
  config_sha256: h("dac32d1e456a1229b472a4e12a6f6d860639a542c1ae757023df367da28164df"),
  adapter_sha256: None,
  conversion_recipe_sha256: Some(h("9ccc1ac336ed3e2ee971d37377e96abe64a2226f609235ee57ecfe34874df3ce")),
  numerical_validation_sha256: None
}
license: {
  code: Spdx("Apache-2.0"), checkpoint: Spdx("MIT"), redistribution: RequiresReview,
  source_url: "https://huggingface.co/mlx-community/mel-roformer-kim-vocal-2-mlx/tree/64cbfcb004e39430e5f584552c05949440ec39ce",
  review_notes: "Checkpoint was relicensed by its author; training corpus is not disclosed. Do not bundle until provenance review."
}
training: {
  summary: "checkpoint training corpus not disclosed in the pinned model card", sources: [],
  documentation_sha256: h("9ccc1ac336ed3e2ee971d37377e96abe64a2226f609235ee57ecfe34874df3ce")
}
input: { sample_rate_hz: 44100, channels: Stereo, encoding: Float32Le }
execution: {
  chunk_frames: 352800, overlap_frames: 176400, normalization: None,
  backend: Mlx { runtime: "mlx-audio==0.4.3", precision: BFloat16 },
  estimated_peak_memory_bytes: 6442450944, required_accelerators: ["apple-silicon"]
}
output: {
  names: ["vocals", "instrumental"], sample_rate_hz: 44100, channels: Stereo,
  additivity: LinearSumWithResidual { residual_name: "instrumental", maximum_error_parts_per_million: 2 }
}
golden_validations: []
```

The MLX runtime sdist must also be locked: `mlx_audio-0.4.3.tar.gz`, SHA-256 `8e87badf56a0f73bf91e3797b1195c01440a181cf0b64a2a08dc1bda4b037f54`. Before enabling the adapter, Audec must create its own fixture/output golden hash; the model card's reported 66.08 dB PyTorch↔MLX SDR is evidence, not an Audec validation artifact.

### `basic-pitch-coreml-0.4.0`

```text
schema_version: 1
model_id: "basic-pitch-coreml-0.4.0"
architecture: { family: "basic-pitch", version: "spotify-fa5997af0a8210982619003269994a1be25eddf3" }
revision: Release {
  version: "pypi-0.4.0",
  source_hash: h("6f48ac4b909c990fd59460622137a03b857c829bb1f3e65c71708da576ab68e5")
}
artifacts: {
  weights_sha256: h("691a6b63c7ddcdde0ee131ff3986dcb1250df47cd738612efde966ba9b4c99cd"),
  config_sha256: h("af7bf7d49bc167e0bf0c30aa2ca6b432c3e10df048d2dd4173ff3a738c020858"),
  adapter_sha256: None, conversion_recipe_sha256: None, numerical_validation_sha256: None
}
license: {
  code: Spdx("Apache-2.0"), checkpoint: Spdx("Apache-2.0"), redistribution: RequiresReview,
  source_url: "https://github.com/spotify/basic-pitch/tree/fa5997af0a8210982619003269994a1be25eddf3",
  review_notes: "Model serializations ship in the Apache repository/package; retain NOTICE and review training-source terms before bundling."
}
training: {
  summary: "paper reports MAESTRO, GuitarSet, MedleyDB-Pitch, iKala, and Slakh-derived supervision", sources: [],
  documentation_sha256: h("308eca019d334104487822197d8fdc81574e9f07a80436d860e044248f26f520")
}
input: { sample_rate_hz: 22050, channels: Mono, encoding: Float32Le }
execution: {
  chunk_frames: 43844, overlap_frames: 7680, normalization: None,
  backend: CoreMl { runtime: "basic-pitch==0.4.0/coreml", precision: Float32 },
  estimated_peak_memory_bytes: 268435456, required_accelerators: ["macOS-CoreML"]
}
output: {
  names: ["onset-map", "note-map", "contour-map", "note-events"],
  sample_rate_hz: 22050, channels: Mono,
  additivity: NonAudio { units: "probability maps and sample-referenced note events" }
}
golden_validations: []
```

The Core ML hashes are respectively `weight.bin` and `model.mlmodel` at the pinned revision. The complete PyPI wheel is SHA-256 `738adb503aae7fdfc7d1e1511aa0ce35052315f260a19531ef4c356708425db0`.

### `beat-this-small0-1.1.0`

```text
schema_version: 1
model_id: "beat-this-small0-1.1.0"
architecture: { family: "beat-this", version: "small0@b95c8ab0c58c2d9fcfd40508ae8dffbc05ac4f5c" }
revision: Release {
  version: "pypi-1.1.0+small0",
  source_hash: h("3017c741f972972a650edcaccfe5760687fe4f5587feaa98896d90f866c2435c")
}
artifacts: {
  weights_sha256: h("6074be2c4d490c5f6101fcc374a1ec72ae93456e23bb6019783b849f5dc7d47b"),
  config_sha256: h("6074be2c4d490c5f6101fcc374a1ec72ae93456e23bb6019783b849f5dc7d47b"),
  adapter_sha256: None, conversion_recipe_sha256: None, numerical_validation_sha256: None
}
license: {
  code: Spdx("MIT"), checkpoint: Spdx("MIT"), redistribution: RequiresReview,
  source_url: "https://cloud.cp.jku.at/public.php/dav/files/7ik4RrBKTS273gp/small0.ckpt",
  review_notes: "Authors explicitly note copyrighted and limited-CC training files; checkpoint embeds its hyperparameters."
}
training: {
  summary: "mixed public/copyrighted beat datasets documented by Beat This!; exact annotations release v1.0", sources: [],
  documentation_sha256: h("be7b51a9b6ff2041fdde81061d079c29451583f2c06c9733bece9282cc7afab0")
}
input: { sample_rate_hz: 22050, channels: Mono, encoding: Float32Le }
execution: {
  chunk_frames: 661500, overlap_frames: 5292, normalization: None,
  backend: Cpu { runtime: "beat-this==1.1.0/torch-cpu", precision: Float32 },
  estimated_peak_memory_bytes: 536870912, required_accelerators: []
}
output: {
  names: ["beat-logits", "downbeat-logits", "beat-events", "downbeat-events"],
  sample_rate_hz: 22050, channels: Mono,
  additivity: NonAudio { units: "frame probabilities and sample-referenced events" }
}
golden_validations: []
```

`config_sha256` deliberately equals the checkpoint hash because the Lightning checkpoint embeds the hyperparameters; the installer should later canonicalize those parameters into a separate config artifact and update the manifest.

### Additional exact artifact locks

These are ready for adapter manifests after their execution contracts and goldens are measured:

| Adapter | Exact source/artifact | SHA-256 / revision |
|---|---|---|
| HTDemucs four-stem MLX | `model.safetensors` | `50d904834f50980e1065f8727805211eed9ac94eb74ff8d0bd60211a4349b2f0`, HF revision `9a32d8a73da0d6182a8a33bda927b7ea46930e44` |
| Inverse Drum Machine | checkpoint / `model.yaml` | `5856a9bee7c6d503842795756d238dc8470f6f3e010e9e4f33ede0362850cb4c` / `2300e60bfaa8c4c4756568d8ede8992d2e56231b2a519acf30fda1d1bb36f2e6`, source commit `456656868538205ef756912c7cf5b0fd936de8af` |
| FM-SynAPSE held-out | `held-out/model.safetensors` | `ec976c84b66396ecfc553f4693f73549e73816a50929ea8d2e4f8eced4fe6554`, HF revision `4b0c2d08265994670eb890efd9b251c4171cc525` |
| Sony SampleID | `sampleid-best.ckpt`, Zenodo 17413869 | Zenodo publishes MD5 `c50d08e54b37e49fbcf8904b1f0f5ca3`; Audec must download once, compute SHA-256, and never treat MD5 as `ContentHash` |
| All-In-One single fold | `harmonix-fold0-0vra4ys2.pth` | `0db596dfb0995f41d62f6267d76a9d54c046f1649bd35e1dbeca0c5f9a7b8acd`, HF revision `379e5fd010b3fdd0ee8381ff8cbcfa51d70b5c19` |
| Open-Unmix runtime | `openunmix-1.3.0.tar.gz` | `cc9245ce728700f5d0b72c67f01be4162777e617cdc47f9b035963afac180fc8` |

## Worker and artifact requirements

Rust/GPUI remains the supervisor. Heavy models execute in cancellable arm64 workers: Core ML first for Basic Pitch, MLX for audited conversions, and version-pinned Python CPU/MPS environments for research adapters. ONNX/Core ML is a per-model optimization, not a universal contract.

```text
hello / capabilities
load_model(manifest_hash)
analyze(material_hash, exact_sample_span, prompts?, references?, masks?, parameters)
progress(job, completed_chunks, total_chunks)
complete(job, staged_artifacts, measurements)
error(job, kind, detail)
cancel(job)
```

Large PCM, masks, tensors, embeddings, MIDI, presets, and waveforms travel through job-scoped files or shared memory, never base64 JSON. The supervisor publishes staged artifacts atomically only after all hashes and lengths validate. A crash/OOM cannot take down GPUI or the realtime transport.

Manifest schema v1 still has one coarse `OutputContract` for a job, but the
landed wire and claim path now carries an `ArtifactDescriptor` per result with
`kind`, media/schema type, time base, additivity, schema revision, and exact
source backlinks. Mixed-output models such as Inverse Drum Machine, StemFX,
MuScriptor, and Syntheon must use those descriptors:

- keep the manifest-level contract conservative and describe each waveform,
  event map, MIDI file, preset, embedding, or control curve independently;
- use a versioned Audec ontology MIME/type identifier for every structured
  sidecar;
- never conceal a preset/event file inside an output name that implies audio;
- include every sidecar hash and schema revision in the job cache identity.

The remaining schema gap is semantic rather than transport-level: adapter
schemas still need typed roles for multiple reference inputs and richer units
for token probabilities, pitch bends, effect automation, and renderer/plugin
fingerprints. Do not flatten those into friendly names or untyped JSON.

Every manifest/installation lock records adapter and weight/config hashes; immutable upstream revision; conversion recipe; numerical validation; audio/resampling/chunk/normalization contracts; peak memory and accelerator; output ontology/additivity; code, checkpoint, and training-data licensing; and backend-specific golden results.

Cache identity includes source-audio hash, exact sample span, model/manifest/artifact hashes, prompts/references/masks, backend/runtime, postprocessing, random seed, resampler, and every effective parameter.

## Claim and provenance semantics

For each result Audec preserves:

- raw target, exact model-authored label, ontology/version, confidence, and time resolution;
- complement/residual when mathematically meaningful;
- mixture consistency and reconstruction error;
- masks, event maps, control curves, and exact sample backlinks;
- competing or overlapping candidates;
- whether audio was masked, directly estimated, or generated;
- model, weight, runtime, conversion, prompt, and user-edit provenance;
- audible reconstruction and residual after accepted edits.

Eight independent text queries do not become eight asserted stems. “Vocal” means “this model's vocal Claim under this exact recipe.” A human can accept, reject, split, merge, relabel, edit events/curves, or use it as conditioning evidence for another transform.

Model downloads are opt-in and content-addressed. Audec shows size, runtime, accelerator, code license, weight license, training disclosure, and redistribution status before download. Never execute Hub Python via `trust_remote_code`; install a reviewed pinned adapter and load tensor formats with safe readers where possible. Pickled PyTorch checkpoints run only inside the isolated worker after source and artifact review.

## Evaluation gates

No adapter graduates from “laboratory” to “recommended” without:

1. immutable artifact locks and a passing manifest validation;
2. offline install and repeatable inference after the first download;
3. cancellation, OOM, crash, malformed-output, and partial-write tests;
4. exact sample-offset and chunk-boundary tests;
5. CPU plus relevant Apple backend goldens and measured peak memory;
6. raw output, residual, and sum-error auditing;
7. UI disclosure of overlap/generative semantics;
8. licensing/provenance review separate for code, weights, and training material.

The Silent Shout album is a useful adversarial product suite, but not a redistributable automated fixture. Maintain synthetic and openly licensed CI fixtures for sub-bass versus pitch-diving kick; source-role changes; ratchets/flams/polymeter; harmonized wet vocals; shared reverb/delay tails; known sidechain keys; chorus/flanger/stereo widening; noise above 15 kHz; transformed known samples; and nonlinear masters whose latent sources cannot sum linearly.

Score more than SDR: onset/note F1 with tolerance sweeps, beat/downbeat continuity, calibration, source bleed, residual audibility, pattern edit distance, control-curve error, preset retrieval recall, resynthesis perceptual distance, boundary artifacts, memory, latency, and cancellation time.

## Research watchlist, not dependencies

- [Separate-and-Detect](https://github.com/ddman1101/Separate-and-detect): compelling five-drum-stem generation plus transcription with official weights, but its 10.8 GB primary checkpoints, unpinned VAE/vocoder dependencies, 16 kHz generative output, and CUDA runtime keep it remote/laboratory only.
- [Diff2Mix](https://arxiv.org/abs/2608.05442): differentiable mixing-console parameter distributions are relevant, but the paper is forward mixing from dry stems/reference style, not blind recovery from a master, and no official code was found.
- [SAM-Audio](https://arxiv.org/abs/2512.18099): uniquely flexible, but size/license/runtime make it a remote opt-in experiment.
- [YourMT3+](https://arxiv.org/abs/2407.04822): useful once its official inference surface, exact artifact, license, and Mac benchmark are small and reproducible.
- [MuScriptor](https://arxiv.org/abs/2607.08168): the strongest concrete full-mix transcription candidate found, but its gated noncommercial weights and semitone/instrument-group output keep it a BYO laboratory adapter.
- [SwiftF0](https://arxiv.org/abs/2508.18440): unusually small and useful for isolated continuous pitch trajectories; benchmark it against native YIN/pYIN and retain both when they disagree.
- [LFO Modulation Extraction](https://arxiv.org/abs/2305.13262): a narrow but concrete curve-valued model for chorus/flanger/phaser motion; never generalize it to arbitrary modulation routing.
- [StemFX](https://arxiv.org/abs/2607.15634): a concrete ordered FX-chain generator, but it explains a known original→target pair rather than blindly recovering a dry chain from one master.
- [Music Source Restoration Challenge](https://arxiv.org/abs/2601.04343): the right benchmark for dry-stem hypotheses; current systems are large research pipelines, and even the challenge average was only 0.29 dB Multi-Mel-SNR for percussion.
- [RemFX](https://github.com/mhrice/RemFx): detects/removes chorus, delay, distortion, compression, and reverb, but its ~3.25 GB Zenodo checkpoints are noncommercial and trained for limited source/effect distributions.
- Open-vocabulary [AudioSep](https://arxiv.org/abs/2308.05037) and guide-conditioned [GuideSep](https://arxiv.org/abs/2507.01339): useful alternate Claims, never a disjoint stem ontology.

## Primary references

- [Mel-Band RoFormer paper](https://arxiv.org/abs/2310.01809), [Kim Vocal 2 original](https://huggingface.co/KimberleyJSN/melbandroformer), and [audited MLX conversion](https://huggingface.co/mlx-community/mel-roformer-kim-vocal-2-mlx)
- [Hybrid Transformer Demucs paper](https://arxiv.org/abs/2211.08553), [official implementation](https://github.com/facebookresearch/demucs), and [MLX conversion](https://huggingface.co/jasonvassallo/demucs-htdemucs-mlx)
- [Open-Unmix repository and model license notes](https://github.com/sigsep/open-unmix-pytorch)
- [Beat This! implementation and checkpoints](https://github.com/CPJKU/beat_this)
- [Basic Pitch implementation/model formats](https://github.com/spotify/basic-pitch) and [paper](https://arxiv.org/abs/2203.09893)
- [All-In-One structure analyzer](https://github.com/mir-aidj/all-in-one)
- [Inverse Drum Machine implementation](https://github.com/bernardo-torres/inverse-drum-machine) and [paper](https://arxiv.org/abs/2505.03337)
- [FM-SynAPSE implementation](https://github.com/DBraun/SynAPSE), [checkpoint](https://huggingface.co/davidbraun/fm-synapse-dx7-gnn), and [paper](https://arxiv.org/abs/2608.18226)
- [Sony SampleID implementation](https://github.com/sony/sampleid), [checkpoint](https://zenodo.org/records/17413869), and [paper](https://arxiv.org/abs/2510.11507)
- [AudioSep](https://github.com/Audio-AGI/AudioSep), [GuideSep](https://github.com/YutongWen/GuideSep), and [SAM-Audio](https://github.com/facebookresearch/sam-audio) implementations/licenses
- [Syntheon](https://github.com/gudgud96/syntheon) and [Instrumental](https://github.com/philippbogdan/instrumental) for synth-parameter research
- [MuScriptor implementation](https://github.com/muscriptor/muscriptor), [small model card](https://huggingface.co/MuScriptor/muscriptor-small), and [paper](https://arxiv.org/abs/2607.08168)
- [StemFX implementation](https://github.com/barry-mir/stemfx), [checkpoint](https://huggingface.co/barry-mir/stemfx-bsfilm), and [paper](https://arxiv.org/abs/2607.15634)
- [Music Source Restoration challenge summary](https://arxiv.org/abs/2601.04343) and [official benchmark](https://msrchallenge.com/)
- [Beat This! Rust implementation](https://github.com/danigb/beat-this-rs), [SwiftF0 implementation](https://github.com/lars76/swift-f0), and [LFO extraction implementation](https://github.com/christhetree/mod_extraction)
