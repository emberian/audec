"""Compare two exported masters: how much they differ, and where.

Used by the live scenarios that claim an edit is audible. LEFT, RIGHT and
LABEL come from the environment so a scenario names its own comparison.
"""

import os
import subprocess

import numpy as np


def load(path: str) -> np.ndarray:
    raw = subprocess.run(
        ["sox", path, "-t", "raw", "-e", "float", "-b", "32", "-c", "2", "-"],
        capture_output=True,
    ).stdout
    return np.frombuffer(raw, dtype=np.float32).reshape(-1, 2)


def rms(x: np.ndarray) -> float:
    return float(np.sqrt(np.mean(x**2))) if len(x) else 0.0


left = load(os.environ["LEFT"])
right = load(os.environ["RIGHT"])
frames = min(len(left), len(right))
left, right = left[:frames], right[:frames]
difference = right - left
rate = 44_100
label = os.environ.get("LABEL", "diff")

differing = int((left != right).any(axis=1).sum())
print(f"  {label}: frames {frames}  left rms {rms(left):.5f}  right rms {rms(right):.5f}")
print(f"  {label}: rms(right-left) {rms(difference):.5f}"
      f"   differing frames {differing} of {frames} ({100.0 * differing / max(frames, 1):.2f}%)")

window = 2 * rate
scores = [
    (start / rate, rms(difference[start : start + window]))
    for start in range(0, max(frames - window, 1), window)
]
scores.sort(key=lambda row: -row[1])
print(f"  {label}: loudest 2 s difference windows:",
      [(round(t, 1), round(r, 5)) for t, r in scores[:6]])
