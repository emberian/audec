# Beat This RTen worker

Status: executable opt-in foundation, 2026-09-01. No model material is bundled
or downloaded by Audec.

`audec-beat-this-worker` is the first real implementation of the generic model
worker boundary. It runs the audited Beat This Rust port in a separate process,
retains raw beat and downbeat logits before decoded events, and publishes all
four outputs through the authenticated atomic model store. Those outputs are
competing rhythm evidence. They do not name an instrument, replace native
rhythm hypotheses, alter the project, or silently become a tempo map.

## Exact source and runtime

| Item | Lock |
|---|---|
| Rust port | `danigb/beat-this-rs@089b509247e6fdcec666511c0dcf0d5f39c21e73` |
| deterministic `git archive` SHA-256 | `1b82c99b959b4670d92421d098d592efcd98e18fcbbe4cdbffc5b128f4a48a4e` |
| worker dependency | exact Git revision above, recorded in `Cargo.lock` |
| inference runtime | pure-Rust `rten 0.24.x`, float32 CPU |
| input | mono Float32LE PCM, 22,050 Hz |
| model window | 661,500 input frames (30 s), 5,292-frame overlap |
| logit time base | 50 frames/s |

The port and original implementation are MIT. The ONNX graphs are derived from
MIT checkpoints, but the authors disclose copyrighted and limited-CC training
material. Consequently the registration remains `RequiresReview`, the install
is opt-in, and Audec does not redistribute the graphs. A runtime license does
not improve checkpoint or training-corpus provenance.

## Installed artifact contract

Create `<registry>/beat-this-rten-small-1.0.0/` and place these exact files in
it. `ModelRegistry::verify` checks type, byte length, path containment, and
SHA-256 before a worker can launch.

| Destination | Acquisition at the pinned source revision | Bytes | SHA-256 |
|---|---|---:|---|
| `mel_spectrogram.onnx` | `models/mel_spectrogram.onnx` | 270,742 | `fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9` |
| `beat_this_small.onnx` | `models/beat_this_small.onnx` | 10,555,592 | `a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f` |
| `beat-this-rs.tar` | `git archive --format=tar 089b509247e6fdcec666511c0dcf0d5f39c21e73` | 17,664,000 | `1b82c99b959b4670d92421d098d592efcd98e18fcbbe4cdbffc5b128f4a48a4e` |
| `ckpt2onnx.py` | `scripts/ckpt2onnx.py` | 3,440 | `0b31944968c089a6f0b7869e9eb2c0a8af7b729f255fe8daf4646648baa8171d` |
| `golden_small.json` | `tests/fixtures/golden_small.json` | 8,436 | `fafae275a6df07d0c10f0a0f06622cfa075abe680d052d21337623b5639f7623` |

One explicit acquisition recipe (choose the registry root yourself) is:

```sh
git clone https://github.com/danigb/beat-this-rs.git beat-this-rs
git -C beat-this-rs checkout 089b509247e6fdcec666511c0dcf0d5f39c21e73
AUDEC_BT_REGISTRY_ROOT="$PWD/audec-models"
mkdir -p "$AUDEC_BT_REGISTRY_ROOT/beat-this-rten-small-1.0.0"
cp beat-this-rs/models/mel_spectrogram.onnx \
  beat-this-rs/models/beat_this_small.onnx \
  beat-this-rs/scripts/ckpt2onnx.py \
  beat-this-rs/tests/fixtures/golden_small.json \
  "$AUDEC_BT_REGISTRY_ROOT/beat-this-rten-small-1.0.0/"
git -C beat-this-rs archive --format=tar \
  089b509247e6fdcec666511c0dcf0d5f39c21e73 \
  > "$AUDEC_BT_REGISTRY_ROOT/beat-this-rten-small-1.0.0/beat-this-rs.tar"
shasum -a 256 "$AUDEC_BT_REGISTRY_ROOT/beat-this-rten-small-1.0.0/"*
```

This is documentation, not an installer: a person reviews the source and
training disclosure, chooses the local registry, and verifies the printed
digests against the table before Audec accepts anything.

The source archive authenticates the adapter revision; the conversion script
records how the graph was derived; the upstream event golden records the
Python-reference boundary. None substitutes for Audec's pending raw-logit
golden on an openly distributable audio fixture.

The provider exposes installation as a typed state: `Installed` carries the
canonical manifest hash, canonical model directory, and exact worker launch;
`Unavailable(InstallStatus)` preserves missing, tampered, unsafe, or absent
registry detail. There is no fallback model and no network installer.

Build the worker explicitly:

```sh
cargo build --release --features beat-this-rten-worker \
  --bin audec-beat-this-worker
```

The normal GPUI application and protocol test builds do not compile RTen.

## Output and cancellation contract

Each successful job publishes, in stable order:

1. `beat-logits.json` — immutable raw logits at 50 Hz;
2. `downbeat-logits.json` — immutable raw logits at 50 Hz;
3. `beat-events.json` — minimally decoded relative event times;
4. `downbeat-events.json` — minimally decoded relative event times.

Every descriptor contains the original material digest and exact source-frame
span. The cache key includes the canonical model manifest, material digest,
span, and effective adapter/chunk/postprocessing parameters. Staging is
supervisor-owned and publication remains all-or-nothing.

The audited upstream API currently performs prediction as one public method and
does not expose a per-model-chunk callback. Audec therefore does not pretend it
can cooperatively cancel inside RTen. The protocol reader remains responsive;
on `Cancel`, the worker emits the typed terminal acknowledgement, flushes it,
and exits the isolated process, aborting inference without publishing partial
artifacts. A future upstream chunk callback can replace this escalation without
changing claims, cache identity, or output schemas.

## Verification still required before default selection

- Record Audec-owned hashes for raw logits and events on open PCM fixtures.
- Compare the already-resampled Float32LE path against the upstream Python
  reference, separately from upstream's Rubato-versus-soxr file path.
- Exercise cancellation during a multi-window fixture in the process harness.
- Benchmark peak memory and wall time on supported macOS and Linux targets.
- Re-review graph redistribution and training-data disclosure before any
  installer or bundled distribution is considered.

Until those gates land this is an executable provider, not the default rhythm
authority.
