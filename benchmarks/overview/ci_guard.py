#!/usr/bin/env python3
"""Deterministic convert-regression guard for CI.

Runs `tylertoo overview` on the fixtures-v1 test inputs and checks the
*structural* shape of the output against a committed baseline:
per-level feature and vertex counts, total rows, total vertices, input
features. These are deterministic functions of the input + knobs.

It deliberately does NOT check wall time, peak RSS, or compressed byte
sizes: timing is far too noisy on shared CI runners to gate on, and the
parquet writer is not byte-deterministic (footer stats drift ~25 bytes
run to run). A structural regression here means the tiling/ranking/
simplification logic changed output — exactly what we want a PR to flag.

Usage:
  ci_guard.py --check     # compare to baseline, exit 1 on drift (CI)
  ci_guard.py --update    # regenerate the baseline (run after intended changes)

Env:
  GPQ_BIN        release binary (default target/release/tylertoo)
  FIXTURE_DIR    input dir (default tests/fixtures/realdata)
"""
import json
import os
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.environ.get("GPQ_BIN", os.path.join(ROOT, "target/release/tylertoo"))
FIXTURES = os.environ.get("FIXTURE_DIR", os.path.join(ROOT, "tests/fixtures/realdata"))
BASELINE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ci_baseline.json")

# (label, filename, extra overview args) — one per geometry class.
CASES = [
    ("polygons", "open-buildings.parquet", ["--mode", "duplicating"]),
    ("lines", "road-detections.parquet", ["--mode", "duplicating"]),
    ("admin", "fieldmaps-madagascar-adm4.parquet", ["--mode", "duplicating"]),
]
MIN_Z, MAX_Z = "0", "14"


def signature(report: dict) -> dict:
    """Deterministic structural fingerprint of a convert report."""
    return {
        "input_features": report.get("input_features"),
        "total_rows": report.get("total_rows"),
        "total_vertices": report.get("total_vertices"),
        "levels": [
            {
                "level": lv.get("level"),
                "feature_count": lv.get("feature_count"),
                "vertex_count": lv.get("vertex_count"),
            }
            for lv in report.get("levels", [])
        ],
    }


def run_case(fname: str, extra: list) -> dict:
    src = os.path.join(FIXTURES, fname)
    if not os.path.exists(src):
        raise SystemExit(f"missing fixture: {src}")
    with tempfile.TemporaryDirectory() as td:
        out = os.path.join(td, "ov.parquet")
        rep = os.path.join(td, "rep.json")
        subprocess.run(
            [BIN, "overview", src, out, "--min-zoom", MIN_Z, "--max-zoom", MAX_Z,
             "--report", rep, *extra],
            check=True, capture_output=True,
        )
        return signature(json.load(open(rep)))


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else "--check"
    current = {label: run_case(f, extra) for label, f, extra in CASES}

    if mode == "--update":
        json.dump(current, open(BASELINE, "w"), indent=2, sort_keys=True)
        print(f"wrote baseline: {BASELINE}")
        return 0

    if not os.path.exists(BASELINE):
        raise SystemExit(f"no baseline at {BASELINE}; run --update first")
    base = json.load(open(BASELINE))
    drift = [k for k in current if current.get(k) != base.get(k)]
    if drift:
        print("CONVERT REGRESSION — structural output changed:", ", ".join(drift))
        for k in drift:
            print(f"\n[{k}] baseline vs current:")
            print("  baseline:", json.dumps(base.get(k)))
            print("  current :", json.dumps(current.get(k)))
        return 1
    print(f"convert guard OK ({len(current)} fixtures unchanged)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
