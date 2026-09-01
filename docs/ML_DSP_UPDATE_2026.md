# 2026 ML/DSP procurement update

Status: verified delta, 2026-08-31. This document supplements
[`ML_MODELS.md`](ML_MODELS.md); it does not replace that strategy or its P0–P3
ordering. Discovery used Kagi Search v1. Claims below were then checked only
against primary papers, official repositories, official model cards, release
metadata, and crates.io package metadata.

There are actionable deltas. Separate-and-Detect now has official code and
weights, MuScriptor is a real Apple-local full-mix transcription candidate,
StemFX exposes an ordered effect-chain language, DDSynth-RL adds a reproducible
2026 inverse-synthesis candidate, VST3 is now MIT-licensed, and Clack's August
release fixes a host-side soundness issue. None of those changes justifies
replacing Audec's current first ML slice.

## Procurement decisions

| Area | Decision now | Reason |
|---|---|---|
| Beat/downbeat runtime | Replace the planned PyTorch execution path with a pinned `beat-this-rs`/`rten` adapter after Audec goldens | It preserves raw logits, has committed Python-parity tests, is faster than realtime on an M4, and removes the Python/torch worker environment without weakening isolation or provenance |
| Selected-source pitch | Add SwiftF0 as a tiny alternate continuous-F0 Claim, not a MIDI replacement | The 398 KB MIT ONNX graph is cheap enough for viewport analysis and retains vibrato/glide; native pitch and Basic Pitch remain independent evidence |
| Modulation extraction | Add the wet-only LFO extractor as the first curve-valued effect model | Its narrow chorus/flanger/phaser scope and 16.6 MB artifact are honest and useful; it should emit a bounded control-curve hypothesis, never a recovered routing claim |
| Drum separation + detection | Register Separate-and-Detect as a remote/CUDA laboratory candidate, not P2 local | Official weights now exist, but inference is about 2.5× realtime on an RTX 6000 Ada, uses 16 kHz generative audio, and needs additional unpinned VAE/vocoder artifacts |
| Broad source separation | Keep the pinned HTDemucs MLX and Kim Vocal 2 MLX adapters | SCNet has good published CPU efficiency, and RoFormer has better specialist scores, but no new primary-source artifact has the current adapters' combination of immutable weight lock, licensing trail, and Apple runtime |
| Full-mix transcription | Register MuScriptor small as a BYO/noncommercial laboratory adapter after Basic Pitch | It has official CPU/CUDA/MPS inference and materially stronger real-production training than YourMT3+, but the weights are gated CC BY-NC 4.0 and discard bends, expressive velocity, and raw frame evidence |
| Effect-chain inference | Register StemFX as a P3 reference-conditioned experiment | It emits ordered, editable per-stem effect chains from an original/target pair, but is not blind inversion from one master and has no published Apple benchmark |
| Dry-stem restoration | Adopt MSRBench as an evaluation fixture/ontology, not a runtime dependency | The 2026 challenge is directly aligned with reverse production, but available systems are large research pipelines and transient/percussion restoration remains very weak |
| Inverse synthesis | Add DDSynth-RL to P3 after FM-SynAPSE retrieval, not ahead of it | Official code and checkpoints exist, but the reference path is a large PyTorch worker driving a separately installed Dexed VST3; local Apple runtime is unreported |
| Plugin execution | Implement CLAP first in the existing dedicated-worker seam; pin Clack v0.2 source | The published Clack 0.1.1 crates predate the v0.2 main-thread reentrancy fix |
| VST3 | Make VST3 the second executable format in the same worker protocol | VST SDK 3.8+ is MIT, removing the old proprietary SDK-license blocker; the C++/COM ABI and untrusted plugin code still require isolation |
| Native DSP | Evaluate `rubato` 5.0.0 first; update `rustfft` under goldens; defer broad DSP frameworks | These are narrow improvements at known seams. CPAL and Symphonia updates should follow Rodio/importer requirements rather than force a transitive upgrade |

## Beat This! Rust: change the runtime, not the evidence contract

Audec already has an exact Beat This! manifest and claim adapter in
`src/beat_this.rs`; the remaining execution path should not default to a Python
PyTorch environment. The [Beat This! Rust port](https://github.com/danigb/beat-this-rs)
is MIT at `089b509247e6fdcec666511c0dcf0d5f39c21e73` and exposes the exact outputs
Audec needs: mel frames, raw beat/downbeat logits, and minimally decoded event
times. Its default `rten` backend is pure Rust with no system inference runtime;
an optional ONNX Runtime backend exists only as a parity oracle.

Exact candidate artifacts at that revision are:

| Artifact | Bytes | SHA-256 |
|---|---:|---|
| committed `mel_spectrogram.onnx` | 270,742 | `fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9` |
| committed `beat_this_small.onnx` | 10,555,592 | `a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f` |
| release `model-large/beat_this.onnx` | 83,162,650 | `5f810debe53459b559127fb55bbad40035bb47cc567b20e501670f968c770f02` |

The port's committed fixture reports Python-reference F-measure 1.0 for the
full graph and at least 0.99 for the small graph within the standard ±70 ms
tolerance. On its author's M4 MacBook Pro, 4:32 processed in 4.6 seconds and
13:48 in 12.1 seconds. Those are upstream claims, not Audec goldens. Before
switching the registration backend to `rten`, reproduce event and raw-logit
parity on Audec's open fixtures, record the exact resampler difference
(`rubato` sinc versus Python `soxr`), and test cancellation at 30-second model
chunk boundaries. Keep the current MIT checkpoint/training-corpus review
disposition: a convenient runtime does not change weight provenance.

This is a good example of when the worker boundary is still useful even though
the runtime is safe Rust: cancellation, peak-memory containment, content-hash
authentication, and future model replacement remain valuable. It need not be a
Python worker merely because it is a model.

For Basic Pitch, [NeuralNote](https://github.com/DamRsn/NeuralNote) is the best
native implementation oracle found. It is Apache-2.0 at
`f979e51dfeab54d5921858af39403308ab06e60c`, ships macOS/Linux/Windows builds,
and implements the whole CQT/harmonic-stack → CNN posteriorgrams → note/pitch-
bend decoding path with ONNX Runtime plus RTNeural. Its committed feature graph
and four JSON weight sets total about 828 KB. It also documents why the path is
offline: the low CQT bins need more than one second of context and the CNN adds
about 120 ms. Treat it as a postprocessing/parity reference rather than the
first artifact lock, because the maintainers state that the manual conversion
steps producing those split weights are not currently published. Audec's
official Basic Pitch Core ML artifact remains the reproducible model source.

## SwiftF0: cheap continuous pitch evidence

[SwiftF0](https://github.com/lars76/swift-f0) is MIT at
`64700fce8ef39c2970814bf427ac1d75a2f20d72`. Its committed ONNX artifact is
397,987 bytes with SHA-256
`7e2390db8379cd9e1e2b22828e55b45b57c8559e4c8335678c717dc245c18176`.
It operates at 16 kHz with a 256-sample hop and returns per-frame F0 plus model
confidence over 46.875–2,093.75 Hz. The official runtime is ONNX Runtime CPU;
upstream reports 132 ms for five seconds of audio but does not publish a
consumer-Mac benchmark matrix or training-corpus provenance adequate for
automatic bundling.

Use it only on a selected or sufficiently isolated pitched Claim. Publish the
continuous Hz/semitone curve, confidence, voicing decision, exact frame time
base, and resampling transform. Native CQT/YIN and Basic Pitch contours remain
independent alternatives. Audec may derive notes, bends, vibrato rate/depth,
and glide terms from the curve, but the immutable raw curve must survive those
edits and quantization choices.

## Wet-only LFO extraction: the first learned control curve

The [official LFO Modulation Extraction repository](https://github.com/christhetree/mod_extraction)
is MPL-2.0 at `cef4a1dfc7cacfa7420693a32702e76f77d6be26` and includes its trained
artifacts. The candidate that actually matches finished-audio use is the
wet-only checkpoint, not the paper's stronger dry+wet configuration:

```text
artifact: models/other/lfo_2dcnn_only_wet_sa_25_25__ph_fl_ch_all_2__idmt_4_egfx_clean_44100__epoch_297_step_23840.ckpt
size: 16596076 bytes
sha256: d5ac9de33c9ce2f9d32bf85f144fb3b166337857da446fd27bc608198e567e8a
input: 44.1 kHz wet audio, two-second analysis window
scope: chorus / flanger / phaser LFOs, approximately 0.5–3 Hz
```

The repository provides CPU and GPU environments but no Apple/MPS or
wall-clock benchmark. It is a pickled Lightning checkpoint, so it runs only in
a reviewed worker. Its output becomes a `ControlCurve` with bounded rate,
phase/shape alternatives, source span, and model confidence. It does not prove
which effect, plugin, delay line, or routing produced the motion; the same
curve can modulate several parameters, and source separation can distort it.
Native comb-notch motion, modulation spectra, and stereo-coherence evidence
must remain visible beside it.

## MuScriptor: meaningful full-mix transcription, noncommercial weights

[MuScriptor](https://arxiv.org/abs/2607.08168) is the first 2026 result in this
scan that materially changes the transcription watchlist. Its
[official implementation](https://github.com/muscriptor/muscriptor) is MIT at
`e34b397bf0584e67bfd81dc591c390e6dcb03350` and version 0.3.0 explicitly
supports Linux CPU/CUDA and Apple-Silicon MPS. It processes 16 kHz mono audio
in five-second chunks and autoregressively emits MT3-like note, drum, timing,
tie, and 36-group instrument tokens.

The smallest useful lock is:

```text
model: MuScriptor/muscriptor-small
revision: 8c127f603b807520fa465c838e9bfee8a91ada4e
artifact: model.safetensors
size: 411888600 bytes
sha256: bbd482c786b895cf7d8f44185073d951adae2ebb8a66f82ca84cd1f84569549c
parameters: 103M
runtime: muscriptor==0.3.0; PyTorch CPU/CUDA or MPS float16
```

The 307M medium model is 1,228,144,472 bytes and the 1.4B large model is
5,465,642,136 bytes; neither should be the first adapter. The project reports
the small model as its practical CPU choice and automatic MPS support, but it
does not publish wall-clock, peak-memory, or Intel/Apple comparative numbers.
Measure those before filling an `ExecutionContract` estimate or a golden.

This is not a replacement for Basic Pitch. MuScriptor's released vocabulary
has semitone note pitch, binary note on/off state, grouped instrument labels,
and drum events. It has no pitch-bend curve, expressive velocity, source audio,
or raw onset/note/contour maps. Its instrument output must therefore remain a
model-authored label on an anonymous pitched/event family. In an electronic
mix, “electric guitar” or “synthesizer” may be timbral resemblance rather than
the source that produced the sound. Preserve the raw token stream, chunk/tie
boundaries, conditioning set, and—if the adapter exposes them—per-token logits.

Distribution is the blocker. The code is MIT, but every official model is
gated under CC BY-NC 4.0 plus an additional attestation that the user has the
necessary rights to the input and use. The paper's 11,000-hour real training
set is internal, and the 1.45M-MIDI synthetic set includes commercial sources.
Treat MuScriptor as user-authenticated BYO research material; do not bundle it
or make it the production default. The lighter Apache-2.0 Basic Pitch Core ML
adapter remains P1 for selected/separated Claims and continuous bends.

## StemFX: an effect-chain term generator, not blind recovery

[StemFX](https://arxiv.org/abs/2607.15634) is unusually relevant to Audec's
language direction: it generates a variable-length, ordered chain of named
effects and quantized parameters for each of `vocals`, `bass`, `drums`, and
`other`. The [official implementation](https://github.com/barry-mir/stemfx) is
MIT at `4d7c1b145ced902a77d28d3f390b553378ded8ad`; PyPI 0.2.0 and the model card
are MIT. Its exact paper checkpoint is:

```text
model: barry-mir/stemfx-bsfilm
revision: 89d0125497871e7fbf83b26ab266a81ab43b94b3
artifact: best_checkpoint.pt
size: 114647946 bytes
sha256: 4a1a9879f8e4605679431733af92eb921090afc6ac7d6bffce51cdd6cb69e40f
input: four stereo stems, 44.1 kHz, 10 seconds
output: 512-d style embeddings plus an ordered per-stem FX-chain term
```

Arbitrary mixtures first pass through SCNet. The official adapter pins the
214 MB separator checkpoint as
`b4675b0269809de27172a050e8767a857077635eda1738db0874d63a79f2b6dd`.
The Python API has an explicit CPU path and accepts a Torch device string; it
defaults only to CUDA or CPU and publishes no MPS result. The `.pt` checkpoint
and SCNet code execute only in a reviewed worker. Benchmark CPU and explicit
MPS before claiming Apple viability.

Most importantly, StemFX observes both an original four-stem set and a target
four-stem set. It predicts the effects that transform the former toward the
latter; it does not infer a unique dry chain from one finished master. In
Audec, the right invocation is `reference = current dry construction`,
`target = source-derived stem claims`, followed by rendered A/B and residual.
Its chain becomes a ranked `EffectChainExpression` proposal, not historical
fact. Retain the original/target claim hashes, four-stem separator provenance,
greedy token stream, parameter quantization, exact MultiAFx vocabulary, and
renderer/plugin fingerprints. Automation and shared-bus effects still require
native modulation/echo/sidechain evidence because StemFX's published term is
per-stem and static over one ten-second window.

Training used SCNet pseudo-stems from roughly 105K FMA songs and random chains
of 1–10 effects per stem from MultiAFx's multi-library vocabulary. This is good
for proposing an editable search seed, not proof of exact effect recovery on a
mastered electronic track. The model belongs after Audec can already construct,
render, and compare stems through its own plugin/effect graph.

## Music Source Restoration: adopt the benchmark before the models

The [inaugural Music Source Restoration challenge](https://arxiv.org/abs/2601.04343)
formalizes a closer target than conventional separation: recover eight
unprocessed stems from a produced/degraded master. MSRBench contains 2,000
paired ten-second, 48 kHz stereo validation clips over vocals, guitars,
keyboards, bass, synthesizers, drums, percussion, and orchestral material under
13 mastering/degradation conditions.

Audec should import that ontology and its Multi-Mel-SNR, Zimtohrli, and
FAD-CLAP evaluation protocol for adapter bake-offs. It should not imply the
benchmark makes the inverse unique. The challenge average was 4.59 dB
Multi-Mel-SNR for bass but only 0.29 dB for percussion; the best systems were
large sequential/ensemble separation, dereverberation, and denoising research
pipelines. The [CPJKU implementation](https://github.com/CPJKU/music-source-restoration)
is MIT at `3bae84c2b42235b42e740b0c9a4ee0249510999c`, but resolves checkpoints
through experiment-run identifiers rather than a compact immutable local
artifact set. Keep MSR as a fixture and generative-hypothesis watch item until
an exact, licensed checkpoint has a credible Mac/Linux worker profile.

## Separate-and-Detect: artifact exists, deployment case is still weak

The authors released an [official repository](https://github.com/ddman1101/Separate-and-detect)
and [official Hugging Face model](https://huggingface.co/ddman1101/Separate-and-Detect),
so this is now an artifact audit rather than a paper-only watch item.

Exact release state:

- Source: `ddman1101/Separate-and-detect@0ac2fe6a35a7968d70ecfb8866cad025c350f5bd`, MIT.
- Model repository revision: `5384a5dcf7456ea5876de2b350438f0cfbe34726`, model card license MIT.
- `msgld_ob.ckpt`: 5,398,491,630 bytes; SHA-256
  `cfc681e51395af954edec67921298e8b4099c9222ab433817f675650ad57d19c`.
- `msgld_obtb.ckpt`: 5,398,799,061 bytes; SHA-256
  `ffead7341064caf0d6cb87fa0da1abc7d2259648ddfb3b6bd1848ce37d3dbcd1`.
- The official inference recipe also downloads `vae-ckpt.ckpt` and
  `hifigan-ckpt.ckpt` from Zenodo record `10643148`. The release does not pin
  their SHA-256 values or state a separate artifact-license disposition in the
  model card. They are therefore manifest blockers, not implicit dependencies.

The [paper](https://arxiv.org/abs/2608.01093) and official recipe establish the
runtime contract and limitations:

- input/output operate at 16 kHz;
- output is five generated stems: kick, snare, toms, hi-hats, and cymbals;
- a fixed onset detector converts each generated stem to events;
- a 10-second example takes approximately 25 seconds on an NVIDIA RTX 6000 Ada
  (real-time factor about 2.5); training used one RTX 6000 Ada;
- the authors do not report SDR or SI-SDR because VAE/vocoder reconstruction
  and phase confound waveform metrics;
- the paper explicitly identifies loss of high-frequency detail, especially
  hats and cymbals, as a consequence of 16 kHz operation.

Integration recommendation: if the two auxiliary artifacts are separately
licensed and hashed, implement one cancellable CUDA/remote research adapter for
`msgld_ob.ckpt` first. The paper reports the onset branch as the more stable
transcription choice; `+OB+TB` changes the reconstruction/perceptual tradeoff
rather than dominating it. Publish five `Generative` audio Claims plus raw
onset outputs and exact source spans. Never declare them `LinearSum`, and derive
and expose a residual against the original input. The two 5.4 GB pickled
Lightning checkpoints must execute only in a reviewed isolated worker. Do not
bundle or advertise Apple-local support without a measured port.

## Separation quality and runtime: no replacement procurement

Scores below are paper measurements, not directly interchangeable product
benchmarks. Training data, stem objectives, evaluation aggregation, and
hardware differ.

| System | Primary-source result and runtime | Artifact/license state | Audec decision |
|---|---|---|---|
| [MDX23C DrumSep via OpenMIRLab](https://github.com/openmirlab/mdxnet-infer) | Splits an already isolated drum Claim into kick/snare/toms/hi-hat/ride/crash at 44.1 kHz stereo; the runtime supports CPU/CUDA/Torch-MPS and native MLX, with reported Torch↔MLX max-absolute divergence around `1.4e-6`. | MIT runtime at `c86c9b62a54b57c7462c97aa37e7b2282b5e99a0`; 437,652,699-byte checkpoint SHA-256 `d2a4aa53eb584d21eead358a4e66d1882ad182911be018f052b5da73be9096d0`; config SHA-256 `17d1649a227f841165bdb4c11a42082898192a1ea3ceab7e7e0b9293d6589dd6`. Original weight licensing is undocumented and mirrors conflict. | Excellent Apple-local BYO laboratory bake-off after HTDemucs, but never bundle or call commercially safe until the original checkpoint author supplies terms. Compare it against IDM's editable event/one-shot outputs, not only stem SDR. |
| [SCNet](https://arxiv.org/abs/2401.13276) | MUSDB18-HQ without extra data: 9.00 dB average SDR for 10.08M parameters and 9.69 dB for 41.2M. The selected base sparsity reports CPU RTF 0.669 on one Intel Xeon Platinum 8372HC thread, versus 1.38 reported for HTDemucs in the same paper. | [Official code](https://github.com/starrytong/SCNet) is MIT and links Google Drive checkpoints. The external weight files lack an official immutable checksum and an explicit checkpoint/training license separate from the code. No official Apple benchmark. | Keep as a benchmark candidate. Do not create an install manifest until exact weight hashes and disposition are recorded and an arm64 CPU/MPS/MLX run is measured. |
| [BS-RoFormer](https://arxiv.org/abs/2309.02612) | The MUSDB18-HQ-only six-layer ablation reports 9.80 dB average SDR. The 11.99 dB model used MUSDB18-HQ plus 500 in-house songs and 16 A100-80GB GPUs for four weeks; the paper gives no local inference benchmark. | The primary paper does not publish an authoritative redistributable checkpoint. MIT architecture reimplementations and community weights do not establish a checkpoint license or corpus provenance. | No new procurement. Continue checkpoint-by-checkpoint review rather than selecting the architecture name. |
| [Mel-Band RoFormer](https://arxiv.org/abs/2310.01809) | Strong specialist separation, but the paper reports training on 16 V100 GPUs and gives no consumer-Mac runtime. | The already audited Kim Vocal 2 MLX artifact in `ML_MODELS.md` remains the concrete installable checkpoint; its own adapter contract uses 8-second chunks with 50% overlap. | Keep the current vocal adapter; do not generalize its license or runtime to other RoFormer weights. |
| [HTDemucs](https://arxiv.org/abs/2211.08553) | The extra-data model reports 9.0 dB average SDR. | [Official code](https://github.com/facebookresearch/demucs) is MIT but was archived on 2025-01-01. The pinned MLX conversion in `ML_MODELS.md` remains reproducible; upstream has no 2026 official runtime update. | Retain as the four-stem local baseline. Validate conversion parity and chunk boundaries before considering SCNet or community RoFormer replacements. |

For a fair Audec bake-off, run every candidate over the same 44.1 kHz stereo
fixtures and record wall time, peak resident/GPU memory, chunk-boundary error,
sum error, bleed probes, and downstream event F1. Published SDR alone is not a
procurement gate.

## DDSynth-RL: real artifact, P3 worker experiment

[DDSynth-RL](https://arxiv.org/abs/2608.03032) is a material inverse-synthesis
delta. It uses masked discrete diffusion and GRPO audio rewards to propose
editable Dexed and MIDI parameters through a non-differentiable renderer. The
[official repository](https://github.com/DDSynth-RL/DDSynthRL) is Apache-2.0 at
`8ffbcda268cb17f40a6b20d78bf94d1d8878b430`. Its README declares released
checkpoints and paper CC BY 4.0.

Reference runtime and reproduction boundary:

- Python 3.10.19, PyTorch/Torchaudio 2.9.0, Lightning 2.6.0,
  `dawdreamer==0.8.3`, and `pedalboard==0.9.9`;
- official config renders 44.1 kHz, 5-second examples through a separately
  installed `synth/Dexed.vst3`, with a 512-frame block size;
- discrete diffusion config uses 105 decoding steps;
- the paper/repository reports no inference wall-clock, Apple Silicon result,
  peak memory, or realtime claim;
- Dexed itself is GPL-3.0. Keep it a user-installed external plugin unless a
  separate distribution review approves another arrangement;
- the processed training set is not redistributed. It contains 860,144
  audio/parameter pairs derived from 53,759 preset groups. Upstream SPINVAE-2
  augmentation is AGPL-3.0, and the repository states that the paper's exact
  second-stage generation program is absent, so bit-exact dataset regeneration
  is not currently possible.

The [official model repository](https://huggingface.co/MINNE-WU/DDSynth-RL) is
CC BY 4.0 at revision `2de75f3f223b4f386557df98cb521168fbdbd084`
and publishes its own `SHA256SUMS`. The general multi-reward research candidate
is:

```text
ddsynth_rl_multi_reward.pt
size: 264646769 bytes
sha256: 430d668abfb42a3ceb0b7870addd32b330691bc3a68b0407a37df060f5a45602
```

Seven additional 237–265 MB AR, flow-matching, supervised diffusion, and GRPO
checkpoints are available and hashed upstream. Do not download all eight by
default. Start with the multi-reward checkpoint only, record its embedded
configuration independently, and use the official checksum file to verify it.

Integration recommendation: retain FM-SynAPSE as P2 because retrieval is
smaller, simpler, and already has a defined CPU query. Add DDSynth-RL as an
opt-in P3 worker only after Audec can host Dexed deterministically through the
plugin worker. Return a ranked set of parameter/MIDI candidates with exact
checkpoint, seed, renderer/plugin fingerprint, and A/B render. One-to-many
inverse synthesis is a proposal mechanism, not recovery of the producer's
original preset. Do not load the `.pt` file in the UI process.

## Plugin-host procurement

Audec already has the right seam in `src/plugin.rs`: format-neutral metadata,
scanner provenance and quarantine, stable parameter identities, backend
capabilities, state/latency/tail contracts, and dedicated/shared isolation.
Keep ABI types behind that boundary.

### CLAP first

- [CLAP 1.2.10](https://github.com/free-audio/clap/tree/195b42a004144fab0b3cf95e9c067187d15365b7)
  is MIT; tag `1.2.10` resolves to commit
  `195b42a004144fab0b3cf95e9c067187d15365b7`.
- [Clack v0.2](https://github.com/prokopyl/clack/releases/tag/v0.2) is dual
  MIT/Apache-2.0 at
  `c5975f9f89f0953b00768680357985d46178078a`, Rust edition 2024, MSRV 1.85.
- crates.io still exposes `clack-host`, `clack-plugin`, and `clack-extensions`
  0.1.1 as the newest published packages in this snapshot. Clack v0.2 changes
  main-thread interfaces from mutable to shared references because the former
  design could cause undefined behavior during host/plugin reentrant calls.

Do not ship the crates.io 0.1.1 host. Pin/vendor the reviewed v0.2 commit or wait
for matching 0.2 crates. Clack is low-level and describes itself as
feature-complete but actively changing; its author states that no functional
higher-level Rust CLAP host is currently available. Implement only the
extensions needed by `PluginMetadata` and execution first: audio/note ports,
parameters, state, render, latency, tail, logging, and thread checks. Defer GUI
bridging.

### VST3 second

[VST SDK 3.8.1 build 84](https://steinbergmedia.github.io/vst3_dev_portal/pages/Versions/Version+3.8.1.html)
was released 2026-08-11. Tag `v3.8.1_build_84` resolves to
`3cdf9ca5d1f5b1b21e0a86832aa4abe55607bd96`. Version 3.8.1 adds
`ITransportControl`, MIDI 2.0 note-expression IDs, and orchestral-articulation
support. [Steinberg's licensing page](https://steinbergmedia.github.io/vst3_dev_portal/pages/VST+3+Licensing/Index.html)
states that VST3 is MIT since 3.8; most SDK files are MIT, while VSTGUI and mda
use BSD-like terms. VST name/logo use remains subject to trademark rules.

The top-level SDK repository is a collection of submodules. A reproducible
adapter must lock every recursive submodule commit (`base`, `cmake`, `doc`,
`pluginterfaces`, `public.sdk`, `tutorials`, and `vstgui4`), not only the
top-level tag. Use a small C++ bridge in the dedicated plugin worker and map it
onto existing Audec-owned contracts. Do not expose Steinberg interfaces through
project state or the GPUI process.

## Rust DSP and audio-runtime queue

Current Audec state matters: `Cargo.toml` asks for `rustfft = "6"` and Rodio
0.22.2; `Cargo.lock` currently resolves RustFFT 6.0.1, CPAL 0.17.3, and
Symphonia 0.5.5. The sampler uses linear interpolation, and offline rendering
currently rejects sample-rate/channel conversion.

| Crate | Exact current release | What changed / where it fits | Decision |
|---|---|---|---|
| [`rubato`](https://github.com/HEnquist/rubato/releases/tag/v5.0.0) | 5.0.0, `MIT OR Apache-2.0`, MSRV 1.87; tag commit `6b72d0f9d8843c6623c818751730764aefcd0525` | Fixes an out-of-bounds panic in asynchronous resamplers during ramped ratio changes, optimizes polynomial interpolation, and adopts `audioadapter` 5.0. | First DSP evaluation. Use for explicit import/offline conversion and, only with preallocated fixed-block adapters, realtime boundaries. Do not silently replace the pitchable sampler's creative interpolation. |
| [`rustfft`](https://crates.io/crates/rustfft/6.4.1) + [`realfft`](https://crates.io/crates/realfft/3.5.0) | RustFFT 6.4.1, `MIT OR Apache-2.0`, MSRV 1.61; RealFFT 3.5.0, MIT | Audec already uses complex FFT planners in analysis, CQT, coverage, and spectral tiles. RealFFT avoids redundant work for real-valued frames. | Update RustFFT only with numerical/performance goldens. Evaluate RealFFT at repeated real-input analysis seams; do not churn one-off paths without a measured win. |
| [`cpal`](https://github.com/RustAudio/cpal/releases/tag/v0.18.2) | 0.18.2, Apache-2.0, MSRV 1.85; tag commit `e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7` | Adds CoreAudio xrun reporting and monotonic timestamps; fixes stale CoreAudio output on partial writes and default-output xrun status. | Valuable telemetry, but Audec receives CPAL through Rodio. Upgrade through a compatible Rodio release or an audited dependency override, then surface xruns in host diagnostics. |
| [`symphonia`](https://github.com/pdeljanov/Symphonia/releases/tag/v0.6.1) | 0.6.1, MPL-2.0, MSRV 1.85; tag commit `ee35874b571a35a9a6e15d3bc9a3aaf8f11fbeee` | Adds sliceable buffers and Opus-in-MP4 channel parsing; hardens demuxers against excessive allocation and fuzz-discovered panics. | Upgrade when importer scope or Rodio compatibility requires it. The current FLAC/WAV/Vorbis/MP3 feature set does not need Opus/MP4 merely because it exists. |
| [`ringbuf`](https://crates.io/crates/ringbuf/0.5.1) | 0.5.1, `MIT OR Apache-2.0` | Ready-made SPSC boundary for realtime/control handoff. | Compare against the semantics actually needed by `src/fifo.rs`; do not add it merely to replace a small history buffer, which is not currently an SPSC queue. |
| [`fundsp`](https://crates.io/crates/fundsp/0.23.0) | 0.23.0, `MIT OR Apache-2.0` | Broad compositional DSP graph useful for experiments and audition prototypes. | Do not make it the project or plugin ABI. Any adoption requires allocation, determinism, denormal, and realtime-thread audits per node. |

`biquad` 0.6.0 is small and permissive but does not displace Audec's existing
filter implementation without response/stability goldens. `dasp` 0.11.0 has not
had a release since 2020 and is not a new foundational dependency.

## Recommended implementation order

1. Make the landed Beat This! claim contract executable with the pinned Rust
   `rten` runtime; preserve raw logits and prove parity/cancellation under
   Audec-owned fixtures.
2. Add Basic Pitch for selected/separated polyphonic material, SwiftF0 as an
   independent continuous-F0 curve, and the wet-only LFO extractor as the
   first learned `ControlCurve` proposal.
3. Run the pinned HTDemucs drum Claim into Inverse Drum Machine, then compile
   its uncertain hits and generated one-shots into editable Audec patterns and
   compare the render/residual.
4. Pin Clack v0.2 in a dedicated CLAP worker and validate scanner quarantine,
   state round-trip, latency/tail, reentrancy, crash recovery, and deterministic
   offline rendering.
5. Add VST3 through the same worker contract using the recursively pinned
   3.8.1 SDK. This unlocks Dexed without importing its ABI into Audec state.
6. Evaluate `rubato` 5.0.0 at the explicit offline resampling seam; then update
   RustFFT/consider RealFFT under existing analysis goldens.
7. Evaluate MuScriptor-small only as user-authenticated noncommercial research;
   evaluate StemFX only after a typed dry construction and deterministic effect
   renderer exist. Use MSRBench as a bake-off, not a product dependency.
8. Run Separate-and-Detect only after hashing and licensing its VAE/vocoder
   dependencies; retain it as generative CUDA evidence, not a local separator.
9. Run DDSynth-RL only after the plugin worker can fingerprint and render the
   user's Dexed installation. Compare its candidate sets against FM-SynAPSE plus
   Audec-owned optimization before promoting it.

No source dependency should float on a branch, a Hub `main` alias, a Google
Drive filename, or an unexpanded SDK superproject tag. Every installed model,
runtime, and plugin bridge must preserve the existing content-addressed worker
and provenance contracts.
