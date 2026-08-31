# 2026 ML/DSP procurement update

Status: verified delta, 2026-08-31. This document supplements
[`ML_MODELS.md`](ML_MODELS.md); it does not replace that strategy or its P0–P3
ordering. Discovery used Kagi Search v1. Claims below were then checked only
against primary papers, official repositories, official model cards, release
metadata, and crates.io package metadata.

There are actionable deltas. Separate-and-Detect now has official code and
weights, DDSynth-RL adds a reproducible 2026 inverse-synthesis candidate, VST3
is now MIT-licensed, and Clack's August release fixes a host-side soundness
issue. None of those changes justifies replacing Audec's current first ML slice.

## Procurement decisions

| Area | Decision now | Reason |
|---|---|---|
| Drum separation + detection | Register Separate-and-Detect as a remote/CUDA laboratory candidate, not P2 local | Official weights now exist, but inference is about 2.5× realtime on an RTX 6000 Ada, uses 16 kHz generative audio, and needs additional unpinned VAE/vocoder artifacts |
| Broad source separation | Keep the pinned HTDemucs MLX and Kim Vocal 2 MLX adapters | SCNet has good published CPU efficiency, and RoFormer has better specialist scores, but no new primary-source artifact has the current adapters' combination of immutable weight lock, licensing trail, and Apple runtime |
| Inverse synthesis | Add DDSynth-RL to P3 after FM-SynAPSE retrieval, not ahead of it | Official code and checkpoints exist, but the reference path is a large PyTorch worker driving a separately installed Dexed VST3; local Apple runtime is unreported |
| Plugin execution | Implement CLAP first in the existing dedicated-worker seam; pin Clack v0.2 source | The published Clack 0.1.1 crates predate the v0.2 main-thread reentrancy fix |
| VST3 | Make VST3 the second executable format in the same worker protocol | VST SDK 3.8+ is MIT, removing the old proprietary SDK-license blocker; the C++/COM ABI and untrusted plugin code still require isolation |
| Native DSP | Evaluate `rubato` 5.0.0 first; update `rustfft` under goldens; defer broad DSP frameworks | These are narrow improvements at known seams. CPAL and Symphonia updates should follow Rodio/importer requirements rather than force a transitive upgrade |

## Separate-and-Detect: artifact exists, deployment case is still weak

The statement in `ML_MODELS.md` that no official runnable artifact was found is
now stale. The authors released the [official repository](https://github.com/ddman1101/Separate-and-detect)
and [official Hugging Face model](https://huggingface.co/ddman1101/Separate-and-Detect).

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

1. Keep the current `ML_MODELS.md` P0/P1 slice unchanged.
2. Pin Clack v0.2 in a dedicated CLAP worker and validate scanner quarantine,
   state round-trip, latency/tail, reentrancy, crash recovery, and deterministic
   offline rendering.
3. Add VST3 through the same worker contract using the recursively pinned
   3.8.1 SDK. This unlocks Dexed without importing its ABI into Audec state.
4. Evaluate `rubato` 5.0.0 at the explicit offline resampling seam; then update
   RustFFT/consider RealFFT under existing analysis goldens.
5. Run Separate-and-Detect only after hashing and licensing its VAE/vocoder
   dependencies; retain it as generative CUDA evidence, not a local separator.
6. Run DDSynth-RL only after the plugin worker can fingerprint and render the
   user's Dexed installation. Compare its candidate sets against FM-SynAPSE plus
   Audec-owned optimization before promoting it.

No source dependency should float on a branch, a Hub `main` alias, a Google
Drive filename, or an unexpanded SDK superproject tag. Every installed model,
runtime, and plugin bridge must preserve the existing content-addressed worker
and provenance contracts.
