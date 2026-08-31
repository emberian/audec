#!/usr/bin/env python3
"""Audec's local-only Beat This small0 JSONL worker.

Run this only in an explicitly prepared Python environment.  It deliberately
never passes ``small0`` (a downloader alias) to Beat This: load_model receives
the exact local checkpoint path that the Rust registry has hash-verified.

The worker writes into the supervisor-assigned staging directory and reports
typed, hash-addressed artifacts.  The Rust supervisor remains responsible for
atomic publication, cache ownership, cancellation escalation, and crash/OOM
containment.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import sys
import threading


PROTOCOL_VERSION = 1
WORKER_NAME = "audec-beat-this-worker"
SAMPLE_RATE = 22050
EXPECTED_PARAMETERS = {
    "adapter": "beat-this-jsonl-v1",
    "chunk_frames": 661500,
    "input_encoding": "float32le",
    "input_rate_hz": SAMPLE_RATE,
    "overlap_frames": 5292,
    "postprocess": "minimal-v1",
}


class WorkerError(Exception):
    def __init__(self, kind: str, detail: str):
        self.kind = kind
        self.detail = detail
        super().__init__(detail)


class Wire:
    def __init__(self) -> None:
        self.outbound_sequence = 0
        self.expected_inbound_sequence = 0
        self.write_lock = threading.Lock()

    def receive(self, line: str) -> dict:
        envelope = json.loads(line)
        if envelope.get("protocol_version") != PROTOCOL_VERSION:
            raise WorkerError("protocol", "unsupported protocol version")
        if envelope.get("sequence") != self.expected_inbound_sequence:
            raise WorkerError("protocol", "controller sequence is not contiguous")
        self.expected_inbound_sequence += 1
        return envelope

    def send(self, kind: str, **payload: object) -> None:
        with self.write_lock:
            envelope = {
                "protocol_version": PROTOCOL_VERSION,
                "sequence": self.outbound_sequence,
                "kind": kind,
                **payload,
            }
            print(json.dumps(envelope, separators=(",", ":")), flush=True)
            self.outbound_sequence += 1


def safe_relative(root: Path, value: str) -> Path:
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        raise WorkerError("invalid_input", "worker file path is not normalized and relative")
    candidate = root.joinpath(*path.parts)
    # The lexical test is sufficient because this worker creates staging and
    # only reads supervisor-created files under its current working directory.
    if root not in (candidate, *candidate.parents):
        raise WorkerError("invalid_input", "worker file path escapes sandbox")
    return candidate


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(64 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def descriptor(path: Path, relative: str, kind: str, media_type: str, request: dict) -> dict:
    return {
        "relative_path": relative,
        "sha256": sha256(path),
        "byte_len": path.stat().st_size,
        "kind": kind,
        "media_type": media_type,
        "schema_revision": 1,
        "time_base_hz": None,
        "additivity": "non_audio",
        "source_backlinks": [{
            "material_sha256": request["material_sha256"],
            "start_frame": request["start_frame"],
            "frame_count": request["frame_count"],
        }],
    }


class BeatThisAdapter:
    def __init__(self, checkpoint: Path) -> None:
        if not checkpoint.is_file():
            raise WorkerError("unavailable", f"verified local checkpoint is absent: {checkpoint}")
        try:
            import numpy as np
            import torch
            from beat_this.inference import Audio2Frames
            from beat_this.model.postprocessor import Postprocessor
        except ImportError as error:
            raise WorkerError("unavailable", f"Beat This runtime is not installed: {error}") from error
        self.np = np
        self.torch = torch
        # An absolute existing filename is essential: Beat This's shortname
        # fallback invokes torch.hub's downloader, which Audec forbids.
        self.frames = Audio2Frames(checkpoint_path=str(checkpoint.resolve()), device="cpu", float16=False)
        self.events = Postprocessor(type="minimal")

    def analyze(self, request: dict, root: Path, cancelled: threading.Event) -> list[dict]:
        if request.get("channel_selection"):
            raise WorkerError("invalid_input", "Beat This receives pre-downmixed mono material")
        parameters = request.get("parameters", {})
        normalized = {name: parameter.get("value") for name, parameter in parameters.items()}
        if normalized != EXPECTED_PARAMETERS:
            raise WorkerError("invalid_input", "unexpected Beat This effective parameter contract")
        input_path = safe_relative(root, request["files"]["material"])
        raw = input_path.read_bytes()
        if len(raw) != request["frame_count"] * 4:
            raise WorkerError("invalid_input", "Float32LE material byte count disagrees with frame span")
        if cancelled.is_set():
            raise WorkerError("cancelled", "cancelled before Beat This inference")
        signal = self.np.frombuffer(raw, dtype="<f4").copy()
        # Audio2Frames is the upstream logits interface.  Do not use the CLI
        # or File2Beats: both would throw away raw logits before publication.
        beat_logits, downbeat_logits = self.frames(self.torch.from_numpy(signal), SAMPLE_RATE)
        if cancelled.is_set():
            raise WorkerError("cancelled", "cancelled after Beat This inference")
        beats, downbeats = self.events(beat_logits, downbeat_logits)
        staging = safe_relative(root, request["files"]["staging_directory"])
        # ModelStore creates the empty directory as part of the atomic
        # publication lease; the worker may only populate it.
        staging.mkdir(parents=True, exist_ok=True)
        beat_logits_path = staging / "beat-logits.npy"
        downbeat_logits_path = staging / "downbeat-logits.npy"
        self.np.save(beat_logits_path, beat_logits.detach().cpu().numpy(), allow_pickle=False)
        self.np.save(downbeat_logits_path, downbeat_logits.detach().cpu().numpy(), allow_pickle=False)
        self._write_events(staging / "beat-events.json", beats, request)
        self._write_events(staging / "downbeat-events.json", downbeats, request)
        return [
            descriptor(beat_logits_path, "beat-logits.npy", "measurement", "application/x-npy", request),
            descriptor(downbeat_logits_path, "downbeat-logits.npy", "measurement", "application/x-npy", request),
            descriptor(
                staging / "beat-events.json",
                "beat-events.json",
                "event_map",
                "application/vnd.audec.beat-events+json",
                request,
            ),
            descriptor(
                staging / "downbeat-events.json",
                "downbeat-events.json",
                "event_map",
                "application/vnd.audec.downbeat-events+json",
                request,
            ),
        ]

    @staticmethod
    def _write_events(path: Path, times: object, request: dict) -> None:
        values = []
        for seconds in times:
            seconds = float(seconds)
            values.append({
                "time_seconds": seconds,
                "source_frame": request["start_frame"] + round(seconds * SAMPLE_RATE),
            })
        path.write_text(json.dumps({"schema_revision": 1, "events": values}, separators=(",", ":")))


def fail(wire: Wire, job_id: str, error: WorkerError) -> None:
    if error.kind == "cancelled":
        wire.send("cancelled", job_id=job_id)
    else:
        wire.send("error", error={"job_id": job_id, "kind": error.kind, "detail": error.detail})


def self_test() -> None:
    """Dependency-free schema check for artifacts emitted by the adapter.

    This deliberately calls ``descriptor`` for both event sidecars, which
    catches Python call-shape mistakes before someone installs a checkpoint.
    Keys and spellings mirror the Rust JSON DTO in ``model_wire.rs``.
    """
    import tempfile

    request = {
        "material_sha256": "01" * 32,
        "start_frame": 320,
        "frame_count": 4,
    }
    with tempfile.TemporaryDirectory(prefix="audec-beat-this-self-test-") as directory:
        directory = Path(directory)
        outputs = []
        for name, kind, media_type in [
            ("beat-logits.npy", "measurement", "application/x-npy"),
            ("downbeat-logits.npy", "measurement", "application/x-npy"),
            ("beat-events.json", "event_map", "application/vnd.audec.beat-events+json"),
            ("downbeat-events.json", "event_map", "application/vnd.audec.downbeat-events+json"),
        ]:
            path = directory / name
            path.write_bytes(b"fixture")
            outputs.append(descriptor(path, name, kind, media_type, request))
        assert [output["relative_path"] for output in outputs] == [
            "beat-logits.npy",
            "downbeat-logits.npy",
            "beat-events.json",
            "downbeat-events.json",
        ]
        assert [output["kind"] for output in outputs] == [
            "measurement",
            "measurement",
            "event_map",
            "event_map",
        ]
        for output in outputs:
            assert set(output) == {
                "relative_path", "sha256", "byte_len", "kind", "media_type",
                "schema_revision", "time_base_hz", "additivity", "source_backlinks",
            }
            assert output["additivity"] == "non_audio"
            assert output["source_backlinks"] == [request]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-root", type=Path,
                        help="registry root containing beat-this-small0-1.1.0/small0.ckpt")
    parser.add_argument("--self-test", action="store_true",
                        help="exercise dependency-free output DTO emission and exit")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    if args.model_root is None:
        parser.error("--model-root is required unless --self-test is used")
    checkpoint = args.model_root / "beat-this-small0-1.1.0" / "small0.ckpt"
    wire = Wire()
    adapter = None
    cancelled: dict[str, threading.Event] = {}
    active_jobs: dict[str, threading.Event] = {}
    root = Path.cwd().resolve()
    for line in sys.stdin:
        try:
            message = wire.receive(line)
            kind = message["kind"]
            if kind == "hello":
                wire.send("capabilities", capabilities={
                    "worker_name": WORKER_NAME,
                    "backends": ["cpu"],
                    "maximum_parallel_jobs": 1,
                    "shared_memory": False,
                })
            elif kind == "load_model":
                # Registry verification happened before launch.  Delay Python
                # import/model construction until the explicit load request.
                adapter = BeatThisAdapter(checkpoint)
                wire.send("model_loaded", manifest_sha256=message["manifest_sha256"])
            elif kind in ("analyze", "separate"):
                request = message["request"]
                job_id = request["job_id"]
                if adapter is None:
                    raise WorkerError("unavailable", "load_model must precede analyze")
                if active_jobs:
                    raise WorkerError("unavailable", "worker permits one active job")
                token = cancelled.setdefault(job_id, threading.Event())
                active_jobs[job_id] = token

                def run_job(request=request, job_id=job_id, token=token, adapter=adapter) -> None:
                    try:
                        wire.send("progress", progress={
                            "job_id": job_id, "phase": "analyzing", "completed_chunks": 0, "total_chunks": 1,
                        })
                        artifacts = adapter.analyze(request, root, token)
                        wire.send("progress", progress={
                            "job_id": job_id, "phase": "verifying", "completed_chunks": 1, "total_chunks": 1,
                        })
                        wire.send("complete", result={
                            "job_id": job_id, "cache_key": request["cache_key"], "artifacts": artifacts, "measurements": [],
                        })
                    except WorkerError as error:
                        fail(wire, job_id, error)
                    except Exception as error:
                        fail(wire, job_id, WorkerError("internal", repr(error)))
                    finally:
                        active_jobs.pop(job_id, None)

                threading.Thread(target=run_job, name=f"beat-this-{job_id}", daemon=True).start()
            elif kind == "cancel":
                token = active_jobs.get(message["job_id"])
                if token is not None:
                    # The upstream model exposes cancellation boundaries only
                    # before/after its frame pass; the Rust supervisor may
                    # still escalate a non-cooperative process independently.
                    token.set()
            elif kind == "shutdown":
                return 0
            else:
                raise WorkerError("protocol", f"unexpected controller message: {kind}")
        except WorkerError as error:
            fail(wire, locals().get("job_id", "unknown-job"), error)
        except Exception as error:  # worker remains isolated; expose a typed failure
            fail(wire, locals().get("job_id", "unknown-job"), WorkerError("internal", repr(error)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
