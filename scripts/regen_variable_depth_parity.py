# /// script
# requires-python = ">=3.11"
# dependencies = ["duckdb==1.5.0", "pmtiles==3.8.1"]
# ///
"""Regenerate tests/fixtures/variable-depth/{points.parquet,manifest.json} (#803).

Slices building centroids out of the Bayerische Vermessungsverwaltung
buildings GeoParquet, runs tippecanoe and manby with
--generate-variable-depth-tile-pyramid (plus a flag-off tippecanoe baseline) and
records the tile addresses, the stopped-tile set and the header zooms. The Rust
parity test compares tylertoo's stop set against this manifest.

  uv run scripts/regen_variable_depth_parity.py \\
      --buildings ~/bench-data/792/buildings.parquet \\
      --tippecanoe ~/bench-data/793/tc-src/tippecanoe \\
      --manby ~/bench-data/793/manby-target/release/manby

`--check` rebuilds the manifest in a temp dir and fails if it differs from the
committed one (the tools are deterministic for a fixed input).
"""

import argparse
import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import duckdb
from pmtiles.reader import MmapSource, Reader, all_tiles

ROOT = Path(__file__).resolve().parents[1]
FIX = ROOT / "tests/fixtures/variable-depth"
BUILDINGS_SHA256 = "8006727894b7fc7792fd70345a809784961fdc08779a764d1b5cdb679304093f"
# (xmin, ymin, xmax, ymax, keep rows with row % N == 0): a dense core (Regensburg)
# inside a wide, sparse rural window, so some branches stop early and some do not.
WINDOWS = [(12.1, 48.5, 12.2, 48.55, 4), (11.7, 48.3, 13.7, 49.1, 512)]
MAX_ZOOM = 14
# Defaults except layer, zooms and the flag under test.
# -r1 (no rate dropping) and a 1000-feature cap make small tiles fit while dense
# ones cannot; tippecanoe only stops early where no feature was dropped.
COMMON = ["-l", "default", "-Z0", f"-z{MAX_ZOOM}", "-r1", "-O", "1000", "--drop-densest-as-needed", "-f"]
VD = "--generate-variable-depth-tile-pyramid"


def sha256(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def make_inputs(buildings: Path, tmp: Path) -> Path:
    if sha256(buildings) != BUILDINGS_SHA256:
        sys.exit(f"{buildings}: sha256 is not the pinned {BUILDINGS_SHA256}")
    con = duckdb.connect()
    con.execute("INSTALL spatial; LOAD spatial; SET geometry_always_xy = true")
    cen = "ST_Centroid(geometry)"
    where = " OR ".join(
        f"(file_row_number % {n} = 0 AND ST_X({cen}) BETWEEN {x0} AND {x1} AND ST_Y({cen}) BETWEEN {y0} AND {y1})"
        for x0, y0, x1, y1, n in WINDOWS
    )
    sel = f"""SELECT file_row_number AS id, {cen} AS geometry
        FROM read_parquet('{buildings}', file_row_number = true) WHERE {where}
        ORDER BY file_row_number"""
    FIX.mkdir(parents=True, exist_ok=True)
    pq = tmp / "points.parquet"
    con.execute(f"COPY ({sel}) TO '{pq}' (FORMAT parquet, COMPRESSION zstd)")
    con.execute(f"COPY (SELECT * FROM '{pq}') TO '{tmp / 'points.geojsonl'}' WITH (FORMAT gdal, DRIVER 'GeoJSONSeq')")
    return pq


def read_archive(p: Path) -> dict:
    with p.open("rb") as f:
        src = MmapSource(f)
        h = Reader(src).header()
        tiles = sorted(zz for zz, _ in all_tiles(src))
    by_zoom: dict[int, list] = {}
    for z, x, y in tiles:
        by_zoom.setdefault(z, []).append([x, y])
    have = set(tiles)
    # A stopped tile has no stored child; at the header max every tile is a leaf.
    stopped = [
        [z, x, y]
        for z, x, y in tiles
        if z < h["max_zoom"] and not any((z + 1, 2 * x + i, 2 * y + j) in have for i in (0, 1) for j in (0, 1))
    ]
    return {
        "header_min_zoom": h["min_zoom"],
        "header_max_zoom": h["max_zoom"],
        "tile_count": len(tiles),
        "tiles_per_zoom": {str(z): len(v) for z, v in sorted(by_zoom.items())},
        "tiles": {str(z): v for z, v in sorted(by_zoom.items())},
        "stopped_tiles": stopped,
    }


def run(name, binary: Path, flag: bool, tmp: Path, version: str) -> dict:
    out = tmp / f"{name}.pmtiles"
    args = COMMON + ([VD] if flag else [])
    subprocess.run([str(binary), "-o", str(out), *args, str(tmp / "points.geojsonl")],
                   check=True, capture_output=True)
    r = {
        "tool": name.split("-")[0],
        "version": version,
        "command": [binary.name, "-o", "OUT.pmtiles", *args, "points.geojsonl"],
        **read_archive(out),
    }
    if not flag:  # baseline for contrast: counts only, no address lists
        del r["tiles"]
    return r


def build(args, tmp: Path) -> dict:
    pq = make_inputs(args.buildings, tmp)
    tc_v = subprocess.run([args.tippecanoe, "--version"], capture_output=True, text=True).stderr.strip() or "tippecanoe v2.79.0"
    mb_v = subprocess.run([args.manby, "--version"], capture_output=True, text=True).stdout.strip()
    runs = {
        "tippecanoe": run("tippecanoe-vd", args.tippecanoe, True, tmp, tc_v),
        "manby": run("manby-vd", args.manby, True, tmp, mb_v),
        "tippecanoe_flag_off": run("tippecanoe-off", args.tippecanoe, False, tmp, tc_v),
    }
    return {
        "input": {
            "file": "points.parquet",
            "sha256": sha256(pq),
            "geojsonl_sha256": sha256(tmp / "points.geojsonl"),
            "rows": duckdb.sql(f"select count(*) from '{pq}'").fetchone()[0],
            "source_buildings_sha256": BUILDINGS_SHA256,
            "slice": "centroids of buildings rows (id = row number) in WINDOWS: [xmin, ymin, xmax, ymax, keep row%N==0]",
            "windows": [list(w) for w in WINDOWS],
        },
        "canonical_maxzoom": MAX_ZOOM,
        "runs": runs,
        "tippecanoe_matches_manby": runs["tippecanoe"]["tiles"] == runs["manby"]["tiles"],
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--buildings", type=Path, required=True)
    ap.add_argument("--tippecanoe", type=Path, required=True)
    ap.add_argument("--manby", type=Path, required=True)
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()
    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        manifest = build(args, tmp)
        text = json.dumps(manifest, separators=(",", ":"), sort_keys=True) + "\n"
        if args.check:
            ok = (FIX / "manifest.json").read_text() == text
            print("manifest up to date" if ok else "manifest DIFFERS")
            return 0 if ok else 1
        (FIX / "manifest.json").write_text(text)
        (FIX / "points.parquet").write_bytes((tmp / "points.parquet").read_bytes())
    print(f"wrote {FIX}: " + ", ".join(f"{k} {v['tile_count']} tiles, {len(v['stopped_tiles'])} stopped, max z{v['header_max_zoom']}" for k, v in manifest["runs"].items()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
