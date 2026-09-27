"""Deterministic convert + export regression guard for CI (#425).

Runs `tylertoo overview` on the fixtures-v1 test inputs, then
`tylertoo export-pmtiles` on each overview, and checks the *structural*
shape of both halves against a committed baseline:

  convert  per-level feature and vertex counts, total rows, total
           vertices, input features (the `overview --report` signature)
  export   per zoom: tile count (from `export-pmtiles --report`, cross-
           checked against `tylertoo stats --json`, which reads only the
           directory), tile feature total as written, and the feature
           total an independent `tylertoo decode --zoom Z` reads back;
           totals over all zooms; and the archive's byte size, compared
           with a tolerance (default 2%, `--tolerance`)

Everything but the byte size is a deterministic function of the input +
knobs and is compared exactly. The byte size is gzip output: it moves
with the compressor and with harmless intra-tile reordering (#343-class
drift), which is why it gets a tolerance instead of an exact pin; an 11%
gzip regression still trips it.

It deliberately does NOT check wall time or peak RSS: timing is far too
noisy on shared CI runners to gate on. A structural regression here means
the tiling/ranking/simplification/encoding logic changed output — exactly
what we want a PR to flag.

Usage:
  ci_guard.py --check                 # compare to baseline, exit 1 on drift (CI)
  ci_guard.py --update                # regenerate the baseline (after intended changes)
  ci_guard.py --check --keep-archives DIR
                                      # also copy each produced <label>.pmtiles
                                      # into DIR for the independent-reader
                                      # checks (scripts/verify_archive.py,
                                      # go-pmtiles verify)

Env:
  GPQ_BIN        release binary (default target/release/tylertoo)
  FIXTURE_DIR    input dir (default tests/fixtures/realdata)
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("GPQ_BIN", ROOT / "target/release/tylertoo"))
FIXTURES = Path(os.environ.get("FIXTURE_DIR", ROOT / "tests/fixtures/realdata"))
BASELINE = Path(__file__).resolve().parent / "ci_baseline.json"

CASES = [
    ("polygons", "open-buildings.parquet", ["--mode", "duplicating"]),
    ("lines", "road-detections.parquet", ["--mode", "duplicating"]),
    ("admin", "fieldmaps-madagascar-adm4.parquet", ["--mode", "duplicating"]),
]
MIN_Z, MAX_Z = "0", "14"
DEFAULT_TOLERANCE = 0.02
ARCHIVE_BYTES_KEY = "archive_bytes"


def run(args: list[str], capture: bool = False) -> str:
    proc = subprocess.run([str(BIN), *args], check=True, capture_output=True, text=True)
    return proc.stdout if capture else ""


def convert_signature(report: dict) -> dict:
    """Deterministic structural fingerprint of an `overview --report`."""
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


def decoded_features(archive: Path, zoom: int, td: Path) -> int:
    """Feature rows `tylertoo decode --zoom Z` reads back from the archive."""
    out = td / f"dec-{zoom}.parquet"
    rep = td / f"dec-{zoom}.json"
    run(["decode", str(archive), str(out), "--zoom", str(zoom), "--report", str(rep)])
    return json.loads(rep.read_text())["features_written"]


def export_signature(archive: Path, export_report: dict, td: Path) -> dict:
    """Structural fingerprint of the export: per-zoom tile counts (report
    and directory agree), written and decoded feature totals, archive bytes."""
    stats = json.loads(run(["stats", str(archive), "--json"], capture=True))
    dir_tiles = {zs["z"]: zs["tile_count"] for zs in stats["per_zoom"]}
    zooms = []
    for zr in export_report["zooms"]:
        z = zr["zoom"]
        if dir_tiles.get(z) != zr["tile_count"]:
            raise SystemExit(
                f"{archive.name}: z{z} export report says {zr['tile_count']} tiles, "
                f"directory holds {dir_tiles.get(z)}"
            )
        zooms.append(
            {
                "zoom": z,
                "tile_count": zr["tile_count"],
                "tile_feature_count": zr["tile_feature_count"],
                "decoded_features": decoded_features(archive, z, td),
            }
        )
    extra = sorted(set(dir_tiles) - {zr["zoom"] for zr in export_report["zooms"]})
    if extra:
        raise SystemExit(f"{archive.name}: directory holds unreported zooms {extra}")
    return {
        "zooms": zooms,
        "total_tiles": export_report["total_tiles"],
        "total_tile_features": export_report["total_tile_features"],
        "decoded_features_total": sum(z["decoded_features"] for z in zooms),
        ARCHIVE_BYTES_KEY: archive.stat().st_size,
    }


def run_case(label: str, fname: str, extra: list[str], keep: Path | None) -> dict:
    src = FIXTURES / fname
    if not src.exists():
        raise SystemExit(f"missing fixture: {src}")
    with tempfile.TemporaryDirectory() as tmp:
        td = Path(tmp)
        ov = td / "ov.parquet"
        conv_rep = td / "convert.json"
        run(
            [
                "overview", str(src), str(ov),
                "--min-zoom", MIN_Z, "--max-zoom", MAX_Z,
                "--report", str(conv_rep), *extra,
            ]
        )  # fmt: skip
        archive = td / f"{label}.pmtiles"
        exp_rep = td / "export.json"
        run(
            [
                "export-pmtiles", str(ov), str(archive),
                "--layer-name", label, "--report", str(exp_rep),
            ]
        )  # fmt: skip
        sig = {
            "convert": convert_signature(json.loads(conv_rep.read_text())),
            "export": export_signature(archive, json.loads(exp_rep.read_text()), td),
        }
        if keep is not None:
            keep.mkdir(parents=True, exist_ok=True)
            shutil.copy2(archive, keep / archive.name)
        return sig


def compare(label: str, base: dict, cur: dict, tolerance: float) -> list[str]:
    """Human-readable drift lines for one case; empty means no drift."""
    drift = []
    if base.get("convert") != cur["convert"]:
        drift.append(
            f"[{label}] convert signature changed\n"
            f"  baseline: {json.dumps(base.get('convert'))}\n"
            f"  current : {json.dumps(cur['convert'])}"
        )
    b_exp = dict(base.get("export") or {})
    c_exp = dict(cur["export"])
    b_bytes, c_bytes = b_exp.pop(ARCHIVE_BYTES_KEY, None), c_exp.pop(ARCHIVE_BYTES_KEY)
    if b_exp != c_exp:
        drift.append(
            f"[{label}] export signature changed\n"
            f"  baseline: {json.dumps(b_exp)}\n"
            f"  current : {json.dumps(c_exp)}"
        )
    if b_bytes is None:
        drift.append(f"[{label}] baseline has no {ARCHIVE_BYTES_KEY}; run --update")
    else:
        rel = abs(c_bytes - b_bytes) / b_bytes
        if rel > tolerance:
            drift.append(
                f"[{label}] archive size {b_bytes} -> {c_bytes} bytes "
                f"({rel:+.2%} vs tolerance {tolerance:.0%})"
            )
    return drift


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument(
        "--check", action="store_true", help="compare to baseline (default)"
    )
    mode.add_argument("--update", action="store_true", help="regenerate the baseline")
    ap.add_argument(
        "--tolerance",
        type=float,
        default=DEFAULT_TOLERANCE,
        help=f"relative archive-size tolerance (default {DEFAULT_TOLERANCE})",
    )
    ap.add_argument(
        "--keep-archives",
        type=Path,
        default=None,
        metavar="DIR",
        help="copy each produced <label>.pmtiles into DIR",
    )
    args = ap.parse_args()
    if not BIN.is_file():
        raise SystemExit(
            f"no binary at {BIN}; run `cargo build --release --package tylertoo`"
        )

    current = {
        label: run_case(label, f, extra, args.keep_archives)
        for label, f, extra in CASES
    }

    if args.update:
        BASELINE.write_text(json.dumps(current, indent=2, sort_keys=True) + "\n")
        print(f"wrote baseline: {BASELINE}")
        return 0

    if not BASELINE.exists():
        raise SystemExit(f"no baseline at {BASELINE}; run --update first")
    base = json.loads(BASELINE.read_text())
    drift = []
    for label, cur in current.items():
        drift.extend(compare(label, base.get(label) or {}, cur, args.tolerance))
    if drift:
        print("CONVERT/EXPORT REGRESSION — structural output changed:")
        for line in drift:
            print(line)
        return 1
    for label, cur in current.items():
        exp = cur["export"]
        base_bytes = base[label]["export"][ARCHIVE_BYTES_KEY]
        print(
            f"  {label:<9} tiles={exp['total_tiles']:<6} "
            f"features={exp['total_tile_features']:<7} "
            f"decoded={exp['decoded_features_total']:<7} "
            f"bytes={exp[ARCHIVE_BYTES_KEY]} (baseline {base_bytes})"
        )
    print(f"convert+export guard OK ({len(current)} fixtures unchanged)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
