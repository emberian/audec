# ML decomposition for electronic music

Electronic production is hostile to a single fixed stem ontology. A synthesizer can function as kick, bass, pad, lead, texture, or all of them across one track. Reverb, distortion, delay, chorus, sidechain compression, and master-bus processing bind several production causes into the same samples. A useful separator is therefore a **hypothesis generator**, not a recovery oracle.

audec should host a model laboratory in which native HPSS/NMF/NMFD, fixed-stem networks, text queries, reference audio, painted masks, and analysis-by-synthesis propose competing audible Claims. A model output may overlap another output. It may be generative rather than additive. It is never silently promoted to an original mixer channel.

## First practical models

### Vocal specialist on Apple Silicon

The best first integration candidate is **Kim Vocal 2 Mel-Band RoFormer**:

- 44.1 kHz stereo vocal/instrumental separation;
- an author-declared MIT checkpoint rather than only MIT architecture code;
- a provenance-linked MLX conversion suitable for an Apple-native worker.

Sources: [original checkpoint](https://huggingface.co/KimberleyJSN/melbandroformer), [MLX conversion](https://huggingface.co/mlx-community/mel-roformer-kim-vocal-2-mlx), [Mel-Band RoFormer paper](https://arxiv.org/abs/2310.01809).

**Open-Unmix UMX/UMX-HQ** should be kept as a smaller, reproducible four-stem baseline and numerical oracle. It is weaker than recent RoFormers, but its official code and pretrained models are explicitly MIT and the architecture has a tractable ONNX path. Source: [official Open-Unmix repository](https://github.com/sigsep/open-unmix-pytorch).

Drum decomposition is valuable for electronic music, but the widely mirrored MDX23C DrumSep checkpoint does not currently have a sufficiently authoritative weight-license/provenance trail for bundled release. It belongs behind bring-your-own-weights experimentation until that changes.

## Open-vocabulary and guided experiments

- **SAM-Audio** accepts text, time-span, visual, and multimodal prompts and returns a target plus residual at 48 kHz. It is a compelling match for queries such as “wet reverb hiss,” “stuttered synth,” or “deep pitch-diving kick.” Its 5–15 GB checkpoints, CUDA-oriented runtime, and restrictive custom license make it an opt-in remote/high-end worker rather than the default. Sources: [official code](https://github.com/facebookresearch/sam-audio), [large checkpoint](https://huggingface.co/facebook/sam-audio-large).
- **AudioSep** is the most accessible current free-form text-query baseline and demonstrates zero-shot separation of described sounds. It is mono 32 kHz, its checkpoint is roughly 1.26 GB, and separately prompted outputs are neither exclusive nor guaranteed to sum to the mixture. Source: [official AudioSep repository](https://github.com/Audio-AGI/AudioSep).
- **GuideSep** is unusually aligned with audec: a user supplies a rough positive/negative time-frequency mask plus a hummed or played guide. The present research model is mono 16 kHz, operates on about 4.1 seconds, excludes drums from its evaluation, and uses training data with noncommercial restrictions. Sources: [code](https://github.com/YutongWen/GuideSep), [checkpoint](https://huggingface.co/YutongCooper/GuideSep-v1), [paper](https://arxiv.org/abs/2507.01339).
- **MVSep Mega 53-stem** exposes an unusually useful electronic taxonomy—synth, keys, kick, snare, hi-hat, toms, percussion, lead/back vocals—but needs about 16 GB VRAM, can be weaker than specialists, and deliberately emits overlapping outputs that do not sum. Its checkpoint license and training provenance are not declared adequately for bundling. Source: [MSST release](https://github.com/ZFTurbo/Music-Source-Separation-Training/releases/tag/v1.0.21).
- **Inverse Drum Machine** jointly transcribes and resynthesizes nine drum classes. That is closer to decompilation than a static stem because it returns editable event/sample hypotheses. The present model is drums-only, mono, trained on six kits, and fragile out of distribution. Sources: [code](https://github.com/bernardo-torres/inverse-drum-machine), [paper](https://arxiv.org/abs/2505.03337).

## Worker boundary

Rust/GPUI remains the supervisor. Heavy models execute in a cancellable arm64 worker, initially PyTorch/MPS for compatibility and MLX for audited conversions. ONNX/Core ML is a per-model optimization, not a universal contract.

A versioned JSONL protocol should provide:

```text
hello / capabilities
load_model(manifest_hash)
separate(material_hash, span, prompt?, references?, masks?, parameters)
progress(job, completed_chunks, total_chunks)
complete(job, staged_artifacts, measurements)
error(job, kind, detail)
cancel(job)
```

Large PCM and masks travel through job-scoped files or shared memory, never base64 JSON. Outputs are staged and atomically published only after completion. A crashed/OOM worker cannot take down GPUI or the audio transport.

Every model manifest records:

- adapter and weight/config hashes, immutable upstream revision, conversion recipe, and numerical validation;
- sample rate, channels, chunking/overlap, normalization, precision, memory estimate, and accelerators;
- output ontology and whether outputs are additive, overlapping, or generative;
- code license, checkpoint-specific license, training-data provenance, redistribution status, and review notes;
- golden-test results for CPU, MPS, and MLX where applicable.

Cache identity includes the source-audio hash, model and artifact hashes, prompt/reference/mask inputs, backend and runtime versions, and every effective parameter.

## Claim semantics

For each result audec preserves:

- the raw target and exact model-authored label;
- its complement or residual, when meaningful;
- mixture consistency and reconstruction error;
- masks and time/sample backlinks;
- competing or overlapping candidates;
- full model/weight/runtime provenance.

Eight independent text queries do **not** become eight asserted stems. “Vocal” means “this model’s vocal Claim under this recipe.” A human can accept, reject, split, merge, relabel, or use it as conditioning evidence for another transform.

## Silent Shout failure suite

The album is a particularly good adversarial evaluation corpus:

- sub-bass versus a pitch-diving kick;
- one synth switching between rhythmic and harmonic roles;
- stutters, ratchets, and flams closer than conventional onset refractory periods;
- layered, harmonized, distorted, and reverberant vocals;
- reverb impulse hiss and delay tails shared by several causes;
- sidechain-pumped pads whose envelope is causally bound to the kick;
- stereo widening and chorus;
- hi-hats and noise sweeps above 15 kHz;
- nonlinear master-bus compression/saturation that prevents estimated causes from summing linearly.

These are not only failure cases. They are the reason audec needs multiple explanatory planes—signal, inference, and experience—instead of a single separator button.
