# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pmtiles==3.8.1",
#   "mapbox-vector-tile==2.2.0",
#   "pyarrow==21.0.0",
#   "shapely==2.1.1",
# ]
# ///
"""
compare_tippecanoe.py - tylertoo vs the pinned tippecanoe on the same inputs,
tolerance-gated (issue #420).

CLAUDE.md says every algorithm must match tippecanoe. This is the check that
enforces it: both tools tile the fixtures-v1 inputs with matched flags, both
archives are decoded tile by tile with the same independent readers (the
Python `pmtiles` reader + `mapbox-vector-tile`, nothing of ours), and per
zoom the two are compared on

  tiles          addressed tiles at z
  features       MVT feature instances at z (a feature clipped into four
                 tiles counts four times - on BOTH sides, so the ratio is
                 fair; corpus/METRICS.md section 2)
  distinct_ids   distinct MVT feature ids at z, where the dataset has a
                 unique integer column carried through as the id (tylertoo
                 `--feature-id`, tippecanoe `--use-attribute-for-id -aI`).
                 This is METRICS.md section 2's dedup-by-id count.
  vertices       coordinate pairs across every decoded geometry at z
  bytes          stored (compressed) tile bytes at z; the total row uses the
                 archive file size
  tile_features  per shared tile (present in both archives), tylertoo's
                 feature count over tippecanoe's; the per-zoom median is
                 gated, p10/p90 are reported

Every metric is a ratio tylertoo / tippecanoe checked against a band
[lo, hi] from tippecanoe_tolerances.toml, which carries a comment per band
saying what was measured and why the band is where it is. A breach exits 1
with the offending cell named in the table; tippecanoe missing or at the
wrong version exits 2.

WHICH TYLERTOO RUN IS GATED
---------------------------
tylertoo's defaults thin features hard at coarse and mid zooms (its density
budget), and tippecanoe's defaults do not - see README.md "Asymmetries" #0
and RESULTS.md section 2. The gated run is therefore the QUALITY-MATCHED one
(`--verbatim --simplify-factor 1.0`, run_e2e.QUALITY_MATCHED_FLAGS): the
thinning ladder off, simplification kept because tippecanoe simplifies too.
That is the run whose output is *meant* to match tippecanoe's, so a drift
there is a parity regression. The defaults run is also measured and printed
(it is the measurement rig #387 asks for: mid-zoom retention vs tippecanoe)
but never gated - its ratios are a product decision, not a bug.

FLAG MAPPING (run_e2e.py's parity set, restated here so this file is
self-contained; README.md "Settings parity" has the reasoning)

  tylertoo tiles                          tippecanoe
  --min-zoom Z --max-zoom Z               -Z Z -z Z
  --layer-name L                          -l L
  --tile-buffer 8                         -b 8        (tippecanoe default 5)
  --max-tile-size 500000                  --maximum-tile-bytes 500000
  (single-pass size valve, always on)     --drop-fraction-as-needed
  --verbatim --simplify-factor 1.0        (defaults: tippecanoe does not
                                           thin lines/polygons by rate)
  --feature-id COL                        --use-attribute-for-id=COL -aI
  (input: GeoParquet)                     (input: line-delimited GeoJSON
                                           written by this script with
                                           pyarrow + shapely, no -P -
                                           tippecanoe cannot read GeoParquet;
                                           see to_geojsonseq)

USAGE
-----
    ./setup_tippecanoe.sh                  # once; builds into <repo>/.tools
    cargo build --release -p tylertoo
    uv run compare_tippecanoe.py           # all fixtures, gate, table
    uv run compare_tippecanoe.py --only open-buildings --json out.json
    uv run compare_tippecanoe.py --summary "$GITHUB_STEP_SUMMARY"

Exit status: 0 within tolerance, 1 on a breach, 2 on a setup problem.
"""

from __future__ import annotations

import argparse
import gzip
import json
import multiprocessing
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path
from typing import NamedTuple

import mapbox_vector_tile
import pyarrow as pa
import pyarrow.parquet as pq
import shapely
import tomllib
from pmtiles.reader import MmapSource, all_tiles
from pmtiles.tile import Compression, deserialize_header

# run_e2e.py (same directory) owns the dataset registry, the flag mapping and
# the pinned-version check; importing it is what keeps the benchmark and this
# gate measuring the same thing.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_e2e

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
TOLERANCES = HERE / "tippecanoe_tolerances.toml"
HEADER_LEN = 127
DECODE_CHUNK = 256

# Datasets with a unique, non-negative integer column that both tools can
# carry through as the MVT feature id, enabling the distinct-id count.
# open-buildings has no unique integer column (boundary_id and s2_id are
# constant) and road-detections has no integer column at all.
ID_COLUMNS = {"madagascar-adm4": "fid"}

METRICS = ("tiles", "features", "distinct_ids", "vertices", "bytes")
MODES = ("matched", "defaults")


class Tools(NamedTuple):
    tylertoo: str
    tippecanoe: str
    workdir: Path
    min_zoom: int
    max_zoom: int
    skip_defaults: bool


# --------------------------------------------------------------------------
# archive decoding (independent readers; identical for both tools)
# --------------------------------------------------------------------------


def _count_vertices(coords) -> int:
    if not coords:
        return 0
    if isinstance(coords[0], (int, float)):
        return 1
    return sum(_count_vertices(c) for c in coords)


def _decode_chunk(job: tuple) -> list[tuple]:
    """Worker: decode a chunk of tiles; return (z, x, y, bytes, features,
    vertices, ids-or-None) per tile."""
    chunk, gzipped, want_ids = job
    out = []
    for z, x, y, data in chunk:
        raw = gzip.decompress(data) if gzipped else data
        decoded = mapbox_vector_tile.decode(raw)
        nfeat = nvert = 0
        ids = [] if want_ids else None
        for layer in decoded.values():
            for feature in layer.get("features", []):
                nfeat += 1
                nvert += _count_vertices(feature["geometry"].get("coordinates"))
                if want_ids and "id" in feature:
                    ids.append(feature["id"])
        out.append((z, x, y, len(data), nfeat, nvert, ids))
    return out


def decode_archive(path: Path, want_ids: bool, jobs: int) -> dict:
    """Per-zoom tiles / features / vertices / bytes / distinct ids, plus the
    per-tile feature counts, for one archive."""
    with path.open("rb") as f:
        get_bytes = MmapSource(f)
        header = deserialize_header(get_bytes(0, HEADER_LEN))
        gzipped = header["tile_compression"] == Compression.GZIP
        if not gzipped and header["tile_compression"] != Compression.NONE:
            raise SystemExit(
                f"{path}: tile compression {header['tile_compression'].name} "
                "is not one this script can decode"
            )
        tiles = [(z, x, y, data) for (z, x, y), data in all_tiles(get_bytes)]

    jobs_list = [
        (tiles[i : i + DECODE_CHUNK], gzipped, want_ids)
        for i in range(0, len(tiles), DECODE_CHUNK)
    ]
    per_zoom: dict[int, dict] = defaultdict(
        lambda: {"tiles": 0, "features": 0, "vertices": 0, "bytes": 0}
    )
    ids_by_zoom: dict[int, set] = defaultdict(set)
    per_tile: dict[int, dict[tuple[int, int], int]] = defaultdict(dict)

    def absorb(results):
        for z, x, y, nbytes, nfeat, nvert, ids in results:
            row = per_zoom[z]
            row["tiles"] += 1
            row["features"] += nfeat
            row["vertices"] += nvert
            row["bytes"] += nbytes
            per_tile[z][(x, y)] = nfeat
            if ids is not None:
                ids_by_zoom[z].update(ids)

    if jobs > 1 and len(jobs_list) > 1:
        with multiprocessing.Pool(jobs) as pool:
            for results in pool.imap_unordered(_decode_chunk, jobs_list):
                absorb(results)
    else:
        for job in jobs_list:
            absorb(_decode_chunk(job))

    for z, row in per_zoom.items():
        row["distinct_ids"] = len(ids_by_zoom[z]) if want_ids else None
    return {
        "archive_bytes": path.stat().st_size,
        "per_zoom": dict(sorted(per_zoom.items())),
        "per_tile": per_tile,
    }


# --------------------------------------------------------------------------
# tolerances
# --------------------------------------------------------------------------


def load_tolerances(path: Path) -> dict:
    with path.open("rb") as f:
        return tomllib.load(f)


def total_band(tol: dict, dataset: str, metric: str) -> list | None:
    """[lo, hi] for the dataset's total row: its own `totals` override if
    any, else the file-level `[totals]`; None when not gated."""
    ds = tol.get("datasets", {}).get(dataset, {}).get("totals", {})
    if metric in ds:
        return ds[metric]
    return tol.get("totals", {}).get(metric)


def zoom_band(tol: dict, dataset: str, metric: str, z: int) -> list | None:
    """[lo, hi] for (dataset, metric) at zoom z from the dataset's
    `zoom_bands` entries (each covers an inclusive `zooms = [lo, hi]` range);
    None when no entry covers z for that metric - informational only."""
    for entry in tol.get("datasets", {}).get(dataset, {}).get("zoom_bands", []):
        lo, hi = entry["zooms"]
        if lo <= z <= hi and metric in entry:
            return entry[metric]
    return None


def in_band(ratio: float | None, band: list | None) -> bool | None:
    """True/False when gated and computable; None when not gated."""
    if band is None or ratio is None:
        return None
    return band[0] <= ratio <= band[1]


def ratio(a, b) -> float | None:
    if a is None or b is None or b == 0:
        return None
    return a / b


# --------------------------------------------------------------------------
# comparison
# --------------------------------------------------------------------------


def percentile(values: list[float], p: float) -> float:
    s = sorted(values)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def tile_feature_ratios(tt: dict, tp: dict) -> dict:
    """Per shared tile at one zoom: tylertoo features / tippecanoe features."""
    shared = [k for k in tp if k in tt and tp[k] > 0]
    ratios = [tt[k] / tp[k] for k in shared]
    return {
        "shared": len(shared),
        "only_tylertoo": len(set(tt) - set(tp)),
        "only_tippecanoe": len(set(tp) - set(tt)),
        "median": statistics.median(ratios) if ratios else None,
        "p10": percentile(ratios, 0.10) if ratios else None,
        "p90": percentile(ratios, 0.90) if ratios else None,
    }


class Gate(NamedTuple):
    """What one comparison is gated against: the dataset's bands (or nothing
    when the tylertoo run is informational)."""

    dataset: str
    tol: dict
    gated: bool

    @property
    def min_reference(self) -> int:
        return self.tol["gate"]["min_reference"]


class Ledger:
    """Pass/fail per metric for one table row, plus the breach messages."""

    def __init__(self):
        self.checks: dict[str, bool | None] = {}
        self.breaches: list[str] = []

    def skip(self, metric: str) -> None:
        self.checks[metric] = None  # informational only

    def check(self, metric: str, where: str, r, band, detail: str) -> None:
        ok = in_band(r, band)
        self.checks[metric] = ok
        if ok is False:
            self.breaches.append(
                f"{where} {metric}: {r:.3f} outside [{band[0]}, {band[1]}] {detail}"
            )


def compare_zoom(gate: Gate, z: int, tt: dict, tp: dict) -> tuple[dict, list[str]]:
    """One per-zoom row: ratios, pass/fail per metric, and the breaches."""
    empty = dict.fromkeys(METRICS, 0)
    a = tt["per_zoom"].get(z, dict(empty))
    b = tp["per_zoom"].get(z, dict(empty))
    row = {"z": z, "tylertoo": a, "tippecanoe": b, "ratios": {}}
    ledger = Ledger()
    for m in METRICS:
        r = ratio(a.get(m), b.get(m))
        row["ratios"][m] = r
        band = zoom_band(gate.tol, gate.dataset, m, z) if gate.gated else None
        ref = b.get(m)
        if band is None or ref is None or ref < gate.min_reference:
            ledger.skip(m)
        else:
            ledger.check(m, f"z{z}", r, band, f"({a.get(m)} vs {ref})")
    tf = tile_feature_ratios(tt["per_tile"].get(z, {}), tp["per_tile"].get(z, {}))
    row["tile_features"] = tf
    m = "tile_features_median"
    band = zoom_band(gate.tol, gate.dataset, m, z) if gate.gated else None
    if band is None or tf["shared"] < gate.min_reference or tf["median"] is None:
        ledger.skip(m)
    else:
        ledger.check(
            m, f"z{z}", tf["median"], band, f"over {tf['shared']} shared tiles"
        )
    row["checks"] = ledger.checks
    return row, ledger.breaches


def compare_totals(
    gate: Gate, rows: list[dict], tt: dict, tp: dict
) -> tuple[dict, list]:
    totals = {"tylertoo": {}, "tippecanoe": {}, "ratios": {}}
    ledger = Ledger()
    for m in METRICS:
        if m == "bytes":
            ta, tb = tt["archive_bytes"], tp["archive_bytes"]
        else:
            va = [r["tylertoo"].get(m) for r in rows]
            vb = [r["tippecanoe"].get(m) for r in rows]
            ta = None if any(v is None for v in va) else sum(va)
            tb = None if any(v is None for v in vb) else sum(vb)
        totals["tylertoo"][m], totals["tippecanoe"][m] = ta, tb
        r = ratio(ta, tb)
        totals["ratios"][m] = r
        band = total_band(gate.tol, gate.dataset, m) if gate.gated else None
        if band is None:
            ledger.skip(m)
        else:
            ledger.check(m, "total", r, band, f"({ta} vs {tb})")
    totals["checks"] = ledger.checks
    return totals, ledger.breaches


def compare_dataset(gate: Gate, tt: dict, tp: dict) -> dict:
    """Every per-zoom and total ratio for one dataset + one tylertoo mode,
    with pass/fail per cell when the gate is on."""
    zooms = sorted(set(tt["per_zoom"]) | set(tp["per_zoom"]))
    rows, breaches = [], []
    for z in zooms:
        row, zb = compare_zoom(gate, z, tt, tp)
        rows.append(row)
        breaches += zb
    totals, tb = compare_totals(gate, rows, tt, tp)
    breaches += tb
    return {"zooms": rows, "totals": totals, "breaches": breaches, "gated": gate.gated}


# --------------------------------------------------------------------------
# running the tools
# --------------------------------------------------------------------------


def run(argv: list[str]) -> None:
    proc = subprocess.run(
        argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, check=False
    )
    if proc.returncode != 0:
        raise SystemExit(
            f"command failed (exit {proc.returncode}): {' '.join(argv)}\n"
            f"{proc.stderr[-2000:]}"
        )


def _is_bbox_struct(field) -> bool:
    if not pa.types.is_struct(field.type):
        return False
    names = {field.type.field(i).name for i in range(field.type.num_fields)}
    return {"xmin", "ymin", "xmax", "ymax"} <= names


def _json_value(v):
    if isinstance(v, (str, int, float, bool)) or v is None:
        return v
    return str(v)  # dates, decimals, nested values: tippecanoe gets a string


def to_geojsonseq(src: Path, workdir: Path, stem: str) -> Path:
    """tippecanoe's input: line-delimited GeoJSON written here with pyarrow +
    shapely, in the parquet's row order.

    Not ogr2ogr and not gpio, which run_e2e.py times, because the gate has to
    produce the SAME tippecanoe input on every machine: Ubuntu's GDAL is
    built without the Parquet driver, and `gpio convert flatgeobuf` fails on
    the madagascar fixture (README.md asymmetry #7). A converter that lives
    in this file has neither problem, and reading the parquet with pyarrow
    and decoding WKB with shapely loses nothing tippecanoe would use: the
    geometry is written at full double precision and every non-geometry
    column becomes a property (non-JSON scalars as strings; a GeoParquet
    bbox covering column is dropped, as GDAL's reader drops it).
    """
    table = pq.read_table(src)
    meta = table.schema.metadata or {}
    geo = json.loads(meta.get(b"geo", b"{}") or b"{}")
    geom_col = geo.get("primary_column", "geometry")
    if geom_col not in table.column_names:
        raise SystemExit(f"{src}: geometry column {geom_col!r} not found")
    prop_cols = [
        f.name for f in table.schema if f.name != geom_col and not _is_bbox_struct(f)
    ]
    wkb = table.column(geom_col).combine_chunks().to_numpy(zero_copy_only=False)
    geoms = shapely.to_geojson(shapely.from_wkb(wkb))
    props = table.select(prop_cols).to_pylist() if prop_cols else [{}] * len(table)

    dst = workdir / f"{stem}.geojsonseq"
    with dst.open("w") as f:
        for p, g in zip(props, geoms, strict=True):
            feature = {
                "type": "Feature",
                "properties": {k: _json_value(v) for k, v in p.items()},
                "geometry": None,
            }
            # Splice the geometry text in verbatim rather than re-parsing it.
            head = json.dumps(feature, separators=(",", ":"))[: -len("null}")]
            f.write(f"{head}{g}}}\n")
    return dst


def build_archives(did: str, ds: dict, tools: Tools) -> dict:
    src = Path(ds["path"])
    layer = ds["layer"]
    id_col = ID_COLUMNS.get(did)
    tt_id = ["--feature-id", id_col] if id_col else []
    tp_id = [f"--use-attribute-for-id={id_col}", "-aI"] if id_col else []
    out = {
        "input": str(src),
        "input_bytes": src.stat().st_size,
        "layer": layer,
        "id_column": id_col,
        "archives": {},
    }

    for mode in MODES:
        if mode == "defaults" and tools.skip_defaults:
            continue
        extra = list(run_e2e.QUALITY_MATCHED_FLAGS) if mode == "matched" else []
        dst = tools.workdir / f"{did}.tylertoo-{mode}.pmtiles"
        report = tools.workdir / f"{did}.tylertoo-{mode}.report.json"
        argv = run_e2e.tylertoo_argv(
            tools.tylertoo,
            src,
            dst,
            layer,
            tools.min_zoom,
            tools.max_zoom,
            report,
            [*extra, *tt_id],
        )
        print(f"  tylertoo ({mode}) ...", flush=True)
        t0 = time.perf_counter()
        run(argv)
        out["archives"][f"tylertoo_{mode}"] = {
            "path": dst,
            "argv": argv,
            "wall_s": time.perf_counter() - t0,
        }

    print("  parquet -> ldGeoJSON ...", flush=True)
    geojson = to_geojsonseq(src, tools.workdir, did)
    out["converter"] = "pyarrow+shapely ldGeoJSON (in-script)"
    dst = tools.workdir / f"{did}.tippecanoe.pmtiles"
    # No -P: parallel parsing reorders features, and tippecanoe's tiny-polygon
    # accumulator is order-dependent, so -P would make the baseline itself
    # non-deterministic between runs.
    argv = run_e2e.tippecanoe_argv(
        tools.tippecanoe,
        geojson,
        dst,
        layer,
        tools.min_zoom,
        tools.max_zoom,
        False,
        tp_id,
    )
    print("  tippecanoe ...", flush=True)
    t0 = time.perf_counter()
    run(argv)
    out["archives"]["tippecanoe"] = {
        "path": dst,
        "argv": argv,
        "wall_s": time.perf_counter() - t0,
    }
    return out


# --------------------------------------------------------------------------
# reporting
# --------------------------------------------------------------------------


def fmt_ratio(r: float | None, ok: bool | None) -> str:
    if r is None:
        return "-"
    s = f"{r:.3f}"
    if ok is False:
        return f"**{s} !**"
    return s


def fmt_int(v) -> str:
    return "-" if v is None else f"{v:,}"


class TableSpec(NamedTuple):
    has_ids: bool
    gated: bool


def _status(checks: dict, gated: bool) -> str:
    vals = [v for v in checks.values() if v is not None]
    if not gated:
        return "info"
    if not vals:
        return "not gated"
    return "OK" if all(vals) else "BREACH"


def _row_cells(label: str, row: dict, spec: TableSpec) -> str:
    a, b, r, checks = row["tylertoo"], row["tippecanoe"], row["ratios"], row["checks"]
    tf = row.get("tile_features")
    c = [
        label,
        f"{fmt_int(a['tiles'])} / {fmt_int(b['tiles'])}",
        fmt_ratio(r["tiles"], checks.get("tiles")),
        f"{fmt_int(a['features'])} / {fmt_int(b['features'])}",
        fmt_ratio(r["features"], checks.get("features")),
    ]
    if spec.has_ids:
        c += [
            f"{fmt_int(a.get('distinct_ids'))} / {fmt_int(b.get('distinct_ids'))}",
            fmt_ratio(r["distinct_ids"], checks.get("distinct_ids")),
        ]
    c += [
        fmt_ratio(r["vertices"], checks.get("vertices")),
        fmt_ratio(r["bytes"], checks.get("bytes")),
    ]
    if tf is None:
        c.append("-")
    elif tf["median"] is None:
        c.append(f"- ({tf['shared']} shared)")
    else:
        c.append(
            f"{fmt_ratio(tf['median'], checks.get('tile_features_median'))} "
            f"({tf['p10']:.2f}-{tf['p90']:.2f}, {tf['shared']})"
        )
    c.append(_status(checks, spec.gated))
    return "| " + " | ".join(c) + " |"


def dataset_table(did: str, mode: str, cmp: dict, has_ids: bool) -> str:
    head = ["z", "tiles tt/tp", "ratio", "features tt/tp", "ratio"]
    if has_ids:
        head += ["distinct ids tt/tp", "ratio"]
    head += ["vertices", "bytes", "tile-median (p10-p90, shared)", "status"]
    spec = TableSpec(has_ids, cmp["gated"])
    lines = [
        f"**{did}** - tylertoo *{mode}* vs tippecanoe"
        + ("" if spec.gated else " (informational, not gated)"),
        "",
        "| " + " | ".join(head) + " |",
        "|" + "---|" * len(head),
    ]
    lines += [_row_cells(f"z{row['z']}", row, spec) for row in cmp["zooms"]]
    lines.append(_row_cells("**total**", cmp["totals"], spec))
    lines.append("")
    lines.append(
        "bytes: per-zoom rows are stored tile bytes, the total row is the "
        "archive file size. tt = tylertoo, tp = tippecanoe; ratios are tt/tp. "
        "`!` marks a cell outside its band."
    )
    return "\n".join(lines)


def strip_paths(obj, workdir: Path):
    blob = json.dumps(obj, indent=2, default=str)
    for prefix, token in ((str(workdir), "<work>"), (str(REPO), "<repo>")):
        blob = blob.replace(prefix, token)
    return blob


def render_report(record: dict, tables: list[str], args) -> str:
    tip = record["tippecanoe"]
    s = record["settings"]
    tol_path = args.tolerances
    if tol_path.is_relative_to(REPO):
        tol_path = tol_path.relative_to(REPO)
    header = [
        "## tylertoo vs tippecanoe parity (#420)",
        "",
        f"tylertoo {record['tylertoo']['version']} ({record['tylertoo']['git_sha']}) "
        f"vs {tip['version']} (pinned {tip['sha']}), "
        f"z{s['min_zoom']}-z{s['max_zoom']}, buffer {s['tile_buffer']}, "
        f"max tile {s['max_tile_bytes']} B. Gated run: tylertoo *{s['gated_mode']}* "
        f"({' '.join(s['quality_matched_flags'])}); tolerances from `{tol_path}`.",
        "",
    ]
    if s["zoom_range_note"]:
        header += [f"> warning: {s['zoom_range_note']}", ""]
    breaches = record["breaches"]
    verdict = (
        "**RESULT: OK** - every gated ratio within its band."
        if not breaches
        else f"**RESULT: {len(breaches)} BREACH(ES)**\n\n"
        + "\n".join(f"- {b}" for b in breaches)
    )
    return "\n".join(header) + verdict + "\n\n" + "\n\n".join(tables) + "\n"


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------


def parse_args():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "--only",
        action="append",
        default=None,
        help="restrict to these dataset ids (repeatable)",
    )
    ap.add_argument("--min-zoom", type=int, default=run_e2e.DEFAULT_MIN_ZOOM)
    ap.add_argument("--max-zoom", type=int, default=run_e2e.DEFAULT_MAX_ZOOM)
    ap.add_argument(
        "--tylertoo",
        default=os.environ.get(
            "TYLERTOO", str(REPO / "target" / "release" / "tylertoo")
        ),
    )
    ap.add_argument(
        "--tippecanoe",
        default=os.environ.get("TIPPECANOE"),
        help="default: the binary in tippecanoe.lock.json; its --version must "
        f"be {run_e2e.EXPECTED_TIPPECANOE_VERSION!r}",
    )
    ap.add_argument("--tolerances", type=Path, default=TOLERANCES)
    ap.add_argument(
        "--skip-defaults",
        action="store_true",
        help="skip the informational tylertoo-defaults run",
    )
    ap.add_argument(
        "--jobs",
        type=int,
        default=os.cpu_count() or 2,
        help="decode worker processes (default: cpu count)",
    )
    ap.add_argument(
        "--work",
        default=None,
        help="working directory for archives (default: a temp dir)",
    )
    ap.add_argument("--keep", action="store_true", help="keep the archives")
    ap.add_argument(
        "--json",
        type=Path,
        default=None,
        help="write the full record (numbers, argv, tolerances) here",
    )
    ap.add_argument(
        "--summary",
        type=Path,
        default=None,
        help="append the markdown tables here (e.g. $GITHUB_STEP_SUMMARY)",
    )
    return ap.parse_args()


def select_datasets(only: list[str] | None) -> dict:
    datasets = dict(run_e2e.DATASETS)
    if only:
        missing = [d for d in only if d not in datasets]
        if missing:
            raise SystemExit(f"error: unknown dataset(s): {', '.join(missing)}")
        datasets = {k: v for k, v in datasets.items() if k in only}
    for did, ds in datasets.items():
        if not Path(ds["path"]).exists():
            raise SystemExit(
                f"error: {did}: {ds['path']} not found (fetch: gh release "
                "download fixtures-v1 --dir tests/fixtures/realdata/)"
            )
    return datasets


def compare_one(did: str, ds: dict, tools: Tools, tol: dict, jobs: int) -> tuple:
    """Build, decode and compare one dataset; returns (record entry, tables,
    gated breaches)."""
    print(f"\n=== {did} ===", flush=True)
    built = build_archives(did, ds, tools)
    want_ids = built["id_column"] is not None
    decoded = {}
    for name, arc in built["archives"].items():
        print(f"  decoding {name} ...", flush=True)
        t0 = time.perf_counter()
        decoded[name] = decode_archive(arc["path"], want_ids, jobs)
        arc["decode_s"] = time.perf_counter() - t0
        arc["output_bytes"] = decoded[name]["archive_bytes"]
    entry = {k: v for k, v in built.items() if k != "archives"}
    entry["runs"] = built["archives"]
    entry["per_zoom"] = {name: d["per_zoom"] for name, d in decoded.items()}
    entry["comparison"] = {}
    tables, breaches = [], []
    gated_mode = tol["gate"]["mode"]
    for mode in MODES:
        key = f"tylertoo_{mode}"
        if key not in decoded:
            continue
        gated = mode == gated_mode
        gate = Gate(did, tol, gated)
        cmp = compare_dataset(gate, decoded[key], decoded["tippecanoe"])
        entry["comparison"][mode] = cmp
        tables.append(dataset_table(did, mode, cmp, want_ids))
        if gated:
            breaches += [f"{did}: {b}" for b in cmp["breaches"]]
            verdict = "BREACH" if cmp["breaches"] else "OK"
            print(
                f"  {mode}: {verdict} ({len(cmp['breaches'])} breach(es))", flush=True
            )
    return entry, tables, breaches


def main() -> int:
    args = parse_args()
    if not Path(args.tylertoo).exists():
        print(
            f"error: tylertoo binary not found: {args.tylertoo}\n"
            "  build it: cargo build --release -p tylertoo",
            file=sys.stderr,
        )
        return 2
    try:
        tip = run_e2e.resolve_tippecanoe(args.tippecanoe)
    except run_e2e.TippecanoeUnavailable as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    tol = load_tolerances(args.tolerances)
    gate_zooms = tol["gate"].get("zoom_range")
    zoom_note = None
    if gate_zooms and list(gate_zooms) != [args.min_zoom, args.max_zoom]:
        zoom_note = (
            f"zoom range z{args.min_zoom}-z{args.max_zoom} differs from the "
            f"tolerance file's z{gate_zooms[0]}-z{gate_zooms[1]}; the bands were "
            "measured at the latter"
        )
        print(f"warning: {zoom_note}", file=sys.stderr)
    try:
        datasets = select_datasets(args.only)
    except SystemExit as exc:
        print(exc, file=sys.stderr)
        return 2

    workdir = Path(args.work) if args.work else Path(tempfile.mkdtemp(prefix="tt-cmp-"))
    workdir.mkdir(parents=True, exist_ok=True)
    tools = Tools(
        args.tylertoo,
        tip["binary"],
        workdir,
        args.min_zoom,
        args.max_zoom,
        args.skip_defaults,
    )
    record = {
        "schema_version": 1,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "tylertoo": {
            "binary": args.tylertoo,
            "version": subprocess.check_output(
                [args.tylertoo, "--version"], text=True
            ).strip(),
            "git_sha": run_e2e.git_sha(),
        },
        "tippecanoe": tip,
        "settings": {
            "min_zoom": args.min_zoom,
            "max_zoom": args.max_zoom,
            "tile_buffer": run_e2e.TILE_BUFFER,
            "max_tile_bytes": run_e2e.MAX_TILE_BYTES,
            "quality_matched_flags": run_e2e.QUALITY_MATCHED_FLAGS,
            "gated_mode": tol["gate"]["mode"],
            "zoom_range_note": zoom_note,
        },
        "tolerances": tol,
        "datasets": {},
    }
    tables: list[str] = []
    breaches: list[str] = []
    try:
        for did, ds in datasets.items():
            entry, t, b = compare_one(did, ds, tools, tol, args.jobs)
            record["datasets"][did] = entry
            tables += t
            breaches += b
    finally:
        if not args.keep and not args.work:
            shutil.rmtree(workdir, ignore_errors=True)
        else:
            print(f"archives kept in {workdir}")

    record["breaches"] = breaches
    record["ok"] = not breaches
    report = render_report(record, tables, args)
    print()
    print(report)
    if args.summary:
        with args.summary.open("a") as f:
            f.write(report)
    if args.json:
        args.json.write_text(strip_paths(record, workdir) + "\n")
        print(f"wrote {args.json}")
    return 1 if breaches else 0


if __name__ == "__main__":
    sys.exit(main())
