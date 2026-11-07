# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""
showcase.py: tylertoo against tippecanoe on a region of global Overture
buildings 2026-10-21.0, on one machine (#810, track D). README.md is the
runbook. Subcommands, in the order a showcase uses them:

    regions    footer-only: estimate buildings per candidate bbox
               (needs pyarrow: `uv run --no-project --with pyarrow`)
    calibrate  time both tool chains on the calibration bbox and project
               every candidate region; print the largest that fits
    extract    the timed gpio extract of one region, then pin it in
               manifest.toml (its sha256), ready for run_manifest
    collect    the results JSON, with provenance, for benchmarks/results/

The timed runs between `extract` and `collect` are run_manifest cells
(manifest.toml): tylertoo straight from s3://, tylertoo on the extract, and
tippecanoe on the extract through tippecanoe_shim.sh (FlatGeobuf, then tile).

Standard library only, apart from `regions`.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE.parent))
import run_manifest as rm

RELEASE = "2026-10-21.0"
BUCKET = "overturemaps-us-west-2"
PREFIX = f"s3://{BUCKET}/release/{RELEASE}/theme=buildings/type=building/"
PARTS_TSV = HERE / f"overture-buildings-{RELEASE}.tsv"
REGIONS_TSV = HERE / "regions.tsv"
MANIFEST = HERE / "manifest.toml"
RESULTS = ROOT / "benchmarks" / "results"
SHIM = HERE / "tippecanoe_shim.sh"
TIPPECANOE = os.environ.get("TIPPECANOE_BIN",
                            "/home/ubuntu/tylertoo-track-666/.tools/tippecanoe-2.79.0/bin/tippecanoe")
TIPPECANOE_VERSION = "v2.79.0"
TIPPECANOE_SHA = "68ab8dcc229f95b8b25877697d5e8d66783af503"
GPIO_VERSION = "1.7.0"
GPIO = f"uvx --from geoparquet-io=={GPIO_VERSION} gpio"
PROPERTIES = ["class", "subtype", "height"]
# corpus/manifest.json polygons-*: tippecanoe's flags for buildings.
TIPPECANOE_FLAGS = "--simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed"
# run_e2e.py's settings parity: z0-14, buffer 8, the same 500,000-byte
# compressed tile cap, no raw cap on tylertoo.
TILE_FLAGS = ["--layer-name", "buildings", "--tile-buffer", "8",
              "--max-compressed-tile-size", "500000", "--max-tile-size", "0"]
ZOOM = (0, 14)
# Anonymous reads: signed requests with whatever credentials the host has
# (an expired SSO session, a role without access) are refused.
S3_ENV = {"AWS_REGION": "us-west-2", "AWS_SKIP_SIGNATURE": "true"}

# The calibration bbox (about 1-3M buildings) and the limits a region must fit.
CALIBRATION = "connecticut"
LIMITS = {"tylertoo_s": 3600.0, "tippecanoe_s": 5400.0, "disk_bytes": 40e9}
# Projections scale linearly in buildings; tippecanoe sorts (n log n) and
# both tools' tile counts grow with area, so each gets a margin.
MARGIN = {"tylertoo": 1.2, "tippecanoe": 1.3}

# Candidate regions: (name, bbox xmin,ymin,xmax,ymax). A state's bbox also
# takes in its neighbors' buildings; the bbox is the region.
CANDIDATES = [
    ("smoke", (-71.53, 41.47, -71.50, 41.50)),
    ("rhode-island", (-71.91, 41.15, -71.12, 42.02)),
    ("connecticut", (-73.73, 40.98, -71.79, 42.05)),
    ("massachusetts", (-73.51, 41.24, -69.93, 42.89)),
    ("new-jersey", (-75.56, 38.93, -73.89, 41.36)),
    ("maryland", (-79.49, 37.91, -75.05, 39.72)),
    ("colorado", (-109.06, 36.99, -102.04, 41.00)),
    ("washington", (-124.85, 45.54, -116.92, 49.00)),
    ("georgia", (-85.61, 30.36, -80.84, 35.00)),
    ("ohio", (-84.82, 38.40, -80.52, 41.98)),
    ("new-york", (-79.76, 40.50, -71.86, 45.02)),
    ("pennsylvania", (-80.52, 39.72, -74.69, 42.27)),
    ("florida", (-87.63, 24.40, -79.97, 31.00)),
    ("california", (-124.48, 32.53, -114.13, 42.01)),
    ("texas", (-106.65, 25.84, -93.51, 36.50)),
    ("netherlands", (3.31, 50.75, 7.23, 53.56)),
    ("belgium", (2.54, 49.50, 6.41, 51.51)),
    ("japan", (122.9, 24.0, 146.0, 45.6)),
    ("germany", (5.87, 47.27, 15.04, 55.06)),
    ("france", (-5.14, 41.33, 9.56, 51.09)),
    ("brazil", (-74.0, -33.8, -34.7, 5.3)),
    ("africa", (-18.0, -35.0, 52.0, 37.5)),
]


# --- parts and regions ------------------------------------------------------


def read_tsv(path: Path) -> list[list[str]]:
    """Rows of a tab-separated file, `#` comment lines skipped."""
    return [ln.split("\t") for ln in path.read_text().splitlines() if ln and not ln.startswith("#")]


def read_parts(path: Path = PARTS_TSV) -> list[dict]:
    """The pinned parts: url, bytes, etag, rows, bbox (its row groups' union)."""
    return [{"url": r[0], "bytes": int(r[1]), "etag": r[2], "rows": int(r[3]),
             "bbox": tuple(map(float, r[4].split(",")))} for r in read_tsv(path)]


def read_regions(path: Path = REGIONS_TSV) -> dict[str, dict]:
    """The candidate table: name -> bbox, estimated rows and bytes."""
    return {r[0]: {"bbox": tuple(map(float, r[1].split(","))), "est_rows": int(r[2]),
                   "est_bytes": int(r[3])} for r in read_tsv(path)}


def intersects(a: tuple, b: tuple) -> bool:
    """Two xmin,ymin,xmax,ymax boxes share at least a point."""
    return a[0] <= b[2] and b[0] <= a[2] and a[1] <= b[3] and b[1] <= a[3]


def overlap_fraction(rg: tuple, box: tuple) -> float:
    """The share of a row group's bbox inside `box` (1 for a degenerate
    bbox that touches it): the estimate takes rows as uniform over their
    row group's bbox."""
    if not intersects(rg, box):
        return 0.0
    area = (rg[2] - rg[0]) * (rg[3] - rg[1])
    if area <= 0:
        return 1.0
    w = min(rg[2], box[2]) - max(rg[0], box[0])
    h = min(rg[3], box[3]) - max(rg[1], box[1])
    return max(0.0, w) * max(0.0, h) / area


def parts_touching(bbox: tuple, parts: list[dict]) -> list[dict]:
    """The parts whose bbox intersects `bbox`: the ones a run may read."""
    return [p for p in parts if intersects(p["bbox"], bbox)]


def bbox_arg(bbox: tuple) -> str:
    """xmin,ymin,xmax,ymax as the CLIs take it."""
    return ",".join(f"{v:g}" for v in bbox)


def regions(_opts) -> None:
    """Estimate rows and bytes per candidate from row-group bbox statistics
    (footers only, anonymous), and refresh the parts' bboxes."""
    from concurrent.futures import ThreadPoolExecutor

    import pyarrow.fs as pafs
    import pyarrow.parquet as pq

    s3 = pafs.S3FileSystem(anonymous=True, region="us-west-2")
    parts = [r[:4] for r in read_tsv(PARTS_TSV)]

    def groups(url: str) -> list[tuple]:
        md = pq.ParquetFile(url.removeprefix("s3://"), filesystem=s3).metadata
        names = [md.schema.column(i).path for i in range(md.num_columns)]
        ix = {k: names.index(f"bbox.{k}") for k in ("xmin", "ymin", "xmax", "ymax")}
        out = []
        for g in range(md.num_row_groups):
            rg = md.row_group(g)
            st = {k: rg.column(i).statistics for k, i in ix.items()}
            out.append(((st["xmin"].min, st["ymin"].min, st["xmax"].max, st["ymax"].max),
                        rg.num_rows, rg.total_byte_size))
        return out

    with ThreadPoolExecutor(32) as ex:
        per_part = list(ex.map(lambda p: groups(p[0]), parts))
    head = [ln for ln in PARTS_TSV.read_text().splitlines() if ln.startswith("#") and "<TAB>" not in ln]
    lines = [*head, "# url<TAB>bytes<TAB>etag<TAB>rows<TAB>bbox (the union of its row groups')"]
    for p, gs in zip(parts, per_part):
        u = (min(g[0][0] for g in gs), min(g[0][1] for g in gs),
             max(g[0][2] for g in gs), max(g[0][3] for g in gs))
        lines.append("\t".join([*p, bbox_arg(u)]))
    PARTS_TSV.write_text("\n".join(lines) + "\n")
    out = ["# Candidate regions: name<TAB>bbox<TAB>est_rows<TAB>est_bytes, from the",
           "# row-group bbox statistics of the pinned parts (rows taken as uniform",
           "# over each row group's bbox). Regenerate: `showcase.py regions`."]
    for name, box in CANDIDATES:
        rows = byts = 0.0
        for gs in per_part:
            for rg, n, b in gs:
                f = overlap_fraction(rg, box)
                rows += f * n
                byts += f * b
        out.append(f"{name}\t{bbox_arg(box)}\t{round(rows)}\t{round(byts)}")
    REGIONS_TSV.write_text("\n".join(out) + "\n")
    print("\n".join(out))


# --- measuring --------------------------------------------------------------


def measure(name: str, scratch: Path, record: Path, argv: list[str], caps: dict,
            env: dict | None = None) -> dict:
    """Run one job the way run_manifest times a cell (its run_capped): GNU
    time, a fresh TYLERTOO_PROFILE_JSON, RSS and scratch (TMPDIR lives in
    `scratch`) sampled and capped. Writes <record>/<name>.json and .log."""
    record.mkdir(parents=True, exist_ok=True)
    (scratch / "tmp").mkdir(parents=True, exist_ok=True)
    prof = record / f"{name}.profile.jsonl"
    prof.unlink(missing_ok=True)
    full = {**os.environ, **(env or {}), "TYLERTOO_PROFILE_JSON": str(prof),
            "TMPDIR": str(scratch / "tmp")}
    res = rm.run_capped(["/usr/bin/time", "-v", *argv], {**caps, "watch": "tmp"}, full,
                        record / f"{name}.log", scratch)
    rec = {"name": name, "argv": argv, **res, "profile": rm.read_profile(prof),
           "bytes_fetched": bytes_fetched(prof), "shim": shim_phases(record / f"{name}.log")}
    (record / f"{name}.json").write_text(json.dumps(rec, indent=2))
    return rec


def bytes_fetched(profile: Path) -> int | None:
    """`pass0.bytes_fetched` of a TYLERTOO_PROFILE_JSON file (remote input)."""
    if not profile.is_file():
        return None
    for ln in profile.read_text().splitlines():
        if ln.strip():
            p0 = json.loads(ln).get("pass0")
            if p0:
                return p0.get("bytes_fetched")
    return None


SHIM_LINE = re.compile(r"^showcase: (.*)$", re.MULTILINE)


def shim_phases(log: Path) -> dict | None:
    """The `showcase: k=v ...` line tippecanoe_shim.sh printed."""
    m = SHIM_LINE.search(log.read_text()) if log.is_file() else None
    return {k: float(v) for k, v in (kv.split("=") for kv in m.group(1).split())} if m else None


def extract_argv(bbox: tuple, dst: Path) -> list[str]:
    """gpio's extract of one bbox from the release: the properties both
    tools tile, the geometry and the bbox covering."""
    return [*GPIO.split(), "extract", "geoparquet", PREFIX + "*.parquet", str(dst),
            "--bbox", bbox_arg(bbox), "--include-cols", ",".join(PROPERTIES)]


def tiles_argv(src: str, dst: Path, bbox: tuple | None = None) -> list[str]:
    """`tiles` arguments with the showcase's flags (and a bbox for s3://)."""
    box = [f"--bbox={bbox_arg(bbox)}"] if bbox else []
    return [src, str(dst), "--min-zoom", str(ZOOM[0]), "--max-zoom", str(ZOOM[1]), *TILE_FLAGS,
            *[f for c in PROPERTIES for f in ("--include-property", c)], *box]


# --- projection -------------------------------------------------------------


METRICS = ["tylertoo_s3_s", "tylertoo_s3_disk", "tylertoo_local_s", "tylertoo_local_disk",
           "extract_s", "extract_bytes", "convert_s", "tile_s", "tippecanoe_disk"]


def at(points: list[dict], key: str, rows: float) -> float:
    """One metric at `rows` from the calibration points: the line through
    the two points (a fixed cost, such as reading all 512 footers, plus a
    per-building cost), never sloping down; through the origin with one."""
    pts = sorted(points, key=lambda p: p["est_rows"])
    big = pts[-1]
    if len(pts) == 1 or big["est_rows"] == pts[0]["est_rows"]:
        return big[key] * rows / big["est_rows"]
    slope = max(0.0, (big[key] - pts[0][key]) / (big["est_rows"] - pts[0]["est_rows"]))
    return big[key] + slope * (rows - big["est_rows"])


def project(points: list[dict], regs: dict[str, dict], limits: dict = LIMITS) -> dict:
    """Project the calibration runs to every candidate region, in estimated
    buildings with each tool's margin, and recommend the largest region
    inside every limit (None when nothing fits).

    points: one dict per calibrated bbox (the smoke bbox and the
    calibration bbox), each with est_rows (the same estimator as the
    candidates) and the METRICS: seconds, and peak scratch bytes
    (tippecanoe_disk is the shim's: FlatGeobuf, temp files, archive)."""
    rows = []
    for name, reg in regs.items():
        v = {k: at(points, k, reg["est_rows"]) for k in METRICS}
        tt, tc = MARGIN["tylertoo"], MARGIN["tippecanoe"]
        p = {"region": name, "bbox": reg["bbox"], "est_rows": reg["est_rows"],
             "tylertoo_s3_s": v["tylertoo_s3_s"] * tt,
             "tylertoo_local_s": v["tylertoo_local_s"] * tt,
             "tippecanoe_e2e_s": (v["extract_s"] + v["convert_s"] + v["tile_s"]) * tc,
             "tylertoo_disk": max(v["tylertoo_s3_disk"], v["extract_bytes"] + v["tylertoo_local_disk"]) * tt,
             "tippecanoe_disk": (v["extract_bytes"] + v["tippecanoe_disk"]) * tc}
        p["fits"] = (max(p["tylertoo_s3_s"], p["tylertoo_local_s"]) <= limits["tylertoo_s"]
                     and p["tippecanoe_e2e_s"] <= limits["tippecanoe_s"]
                     and max(p["tylertoo_disk"], p["tippecanoe_disk"]) <= limits["disk_bytes"])
        rows.append(p)
    rows.sort(key=lambda p: p["est_rows"])
    fit = [p for p in rows if p["fits"] and p["region"] not in ("smoke", CALIBRATION)]
    return {"limits": limits, "margin": MARGIN, "calibration": points, "regions": rows,
            "recommended": fit[-1]["region"] if fit else None}


def format_projection(pj: dict) -> str:
    """The projection as a table, the recommendation last."""
    out = [(f"{'region':16} {'est rows':>13} {'tt s3':>8} {'tt local':>8} {'tc e2e':>8} "
           f"{'tt disk':>8} {'tc disk':>8}  fits")]
    for p in pj["regions"]:
        out.append(f"{p['region']:16} {p['est_rows']:>13,} {p['tylertoo_s3_s'] / 60:>7.0f}m "
                   f"{p['tylertoo_local_s'] / 60:>7.0f}m {p['tippecanoe_e2e_s'] / 60:>7.0f}m "
                   f"{p['tylertoo_disk'] / 1e9:>6.1f}GB {p['tippecanoe_disk'] / 1e9:>6.1f}GB  "
                   f"{'yes' if p['fits'] else 'no'}")
    lim = pj["limits"]
    out.append(f"limits: tylertoo {lim['tylertoo_s'] / 60:.0f} min, tippecanoe end to end "
               f"{lim['tippecanoe_s'] / 60:.0f} min, disk {lim['disk_bytes'] / 1e9:.0f} GB each")
    out.append(f"recommended: {pj['recommended']}")
    return "\n".join(out)


def calibrate_one(region: str, reg: dict, tt: str, work: Path, rss_gib: float) -> dict:
    """Time both chains once on one bbox: the calibration point."""
    work = work / region
    rec = work / "records"
    caps = {"wall_s": 7200, "rss_gib": rss_gib, "scratch_gib": 50}
    ext = work / "extract" / f"{region}.parquet"
    ext.unlink(missing_ok=True)
    jobs = [
        ("extract", work / "extract", extract_argv(reg["bbox"], ext), {}),
        ("tylertoo-s3", work / "s3", [tt, "tiles", *tiles_argv(PREFIX, work / "s3" / "out.pmtiles",
                                                                 reg["bbox"])], S3_ENV),
        ("tylertoo-local", work / "local",
         [tt, "tiles", *tiles_argv(str(ext), work / "local" / "out.pmtiles")], {}),
        ("tippecanoe", work / "tc", [str(SHIM), "tiles", *tiles_argv(str(ext), work / "tc" / "out.pmtiles")],
         {"TIPPECANOE_BIN": TIPPECANOE, "TIPPECANOE_FLAGS": TIPPECANOE_FLAGS, "GPIO": GPIO}),
    ]
    r = {}
    for name, scratch, argv, env in jobs:
        (scratch / "out.pmtiles").unlink(missing_ok=True)
        print(f"calibrate {region} {name}: {' '.join(argv)}", flush=True)
        r[name] = measure(name, scratch, rec, argv, caps, env)
        if r[name]["status"] != "ok":
            raise SystemExit(f"{name} ended {r[name]['status']}; see {rec / (name + '.log')}")
    mib = 1 << 20
    point = {"region": region, "est_rows": reg["est_rows"],
             "rows": (r["tylertoo-local"]["profile"].get("convert") or {}).get("pass1_rows"),
             "extract_s": r["extract"]["wall_s"], "extract_bytes": ext.stat().st_size,
             "tylertoo_s3_s": r["tylertoo-s3"]["wall_s"],
             "tylertoo_s3_disk": r["tylertoo-s3"]["scratch_peak_mib"] * mib,
             "tylertoo_s3_fetched": r["tylertoo-s3"]["bytes_fetched"],
             "tylertoo_local_s": r["tylertoo-local"]["wall_s"],
             "tylertoo_local_disk": r["tylertoo-local"]["scratch_peak_mib"] * mib,
             "convert_s": r["tippecanoe"]["shim"]["fgb_convert_s"],
             "tile_s": r["tippecanoe"]["shim"]["tippecanoe_s"],
             "tippecanoe_disk": r["tippecanoe"]["scratch_peak_mib"] * mib}
    (rec / "point.json").write_text(json.dumps(point, indent=2))
    return point


def calibrate(opts) -> None:
    """Time both chains on the smoke bbox (the fixed costs) and on the
    calibration bbox, then project every candidate and recommend one."""
    regs = read_regions()
    work = Path(opts.work).expanduser().resolve()
    tt = str(Path(opts.tylertoo).resolve())
    points = [calibrate_one(r, regs[r], tt, work, opts.rss_gib) for r in opts.regions.split(",")]
    pj = project(points, regs)
    (work / "projection.json").write_text(json.dumps(pj, indent=2))
    print(format_projection(pj))


# --- extract and pin --------------------------------------------------------


def render_manifest(region: str, bbox: tuple, extract: dict) -> str:
    """manifest.toml for one region: the parts listing and the extract
    pinned, and the s3:// and local cells (run_manifest's format)."""
    listing = PARTS_TSV.relative_to(ROOT).as_posix()
    flags = [*TILE_FLAGS, *[f for c in PROPERTIES for f in ("--include-property", c)]]
    q = lambda xs: "[" + ", ".join(json.dumps(x) for x in xs) + "]"
    return f"""# Generated by `showcase.py extract {region}`; do not edit by hand (README.md).
# The showcase cells (#810, track D), in benchmarks/manifest.toml's format:
#
#   uv run --no-project python -I benchmarks/run_manifest.py \\
#       --manifest benchmarks/showcase/manifest.toml run --cells ... --bin ...
#
# Region `{region}`: bbox {bbox_arg(bbox)} of Overture buildings {RELEASE}.
# Settings parity is run_e2e.py's (z0-14, buffer 8, 500,000-byte compressed
# cap). tippecanoe runs the `ovt-local` cell through tippecanoe_shim.sh.

[defaults]
pairs = 3
warmup = 1
allocator = "any"
wall_s = 21600
rss_gib = 110
scratch_gib = 140
min_free_gb = 5
load_max = 3.0
load_wait_s = 1800

# The listing pins every part by key, size and ETag. The s3:// cell reads
# the parts in place, so the listing is its input.
[inputs.overture-listing]
path = "{listing}"
sha256 = "{rm.sha256_file(PARTS_TSV)}"

[inputs.overture-{region}]
file = "{extract['file']}"
sha256 = "{extract['sha256']}"
bytes = {extract['bytes']}
geometry = "Polygon"
notes = "gpio {GPIO_VERSION} extract geoparquet --bbox {bbox_arg(bbox)} --include-cols {','.join(PROPERTIES)} of {PREFIX}"

[cells.ovt-s3]
label = "Overture buildings {region}: tylertoo straight from s3://"
tickets = [664]
input = "overture-listing"
command = "tiles"
args = ["{PREFIX}", "{{out}}/out.pmtiles"]
output = "out.pmtiles"
zoom = [{ZOOM[0]}, {ZOOM[1]}]
flags = {q([*flags, f"--bbox={bbox_arg(bbox)}"])}
env = {{ AWS_REGION = "us-west-2", AWS_SKIP_SIGNATURE = "true" }}
watch = "tmp"
pairs = 1
warmup = 0
heavy = true

[cells.ovt-local]
label = "Overture buildings {region}: the local extract (tylertoo, or tippecanoe via the shim)"
tickets = [664]
input = "overture-{region}"
command = "tiles"
args = ["{{input}}", "{{out}}/out.pmtiles"]
output = "out.pmtiles"
zoom = [{ZOOM[0]}, {ZOOM[1]}]
flags = {q(flags)}
watch = "tmp"
heavy = true

[cells.ovt-local-bounded]
label = "Overture buildings {region}: tylertoo on the local extract with --profile bounded"
tickets = [664]
input = "overture-{region}"
command = "tiles"
args = ["{{input}}", "{{out}}/out.pmtiles"]
output = "out.pmtiles"
zoom = [{ZOOM[0]}, {ZOOM[1]}]
flags = {q([*flags, "--profile", "bounded"])}
watch = "tmp"
heavy = true
"""


def extract(opts) -> None:
    """The timed extract of one region into <data>/inputs, then the manifest
    pinned to it."""
    reg = read_regions()[opts.region]
    data = Path(opts.data).expanduser().resolve()
    name = f"overture-{RELEASE}-buildings-{opts.region}.parquet"
    dst = data / "inputs" / name
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.unlink(missing_ok=True)
    rec = measure("extract", data / "scratch" / f"{opts.region}-extract",
                  data / "results" / f"{opts.region}-extract", extract_argv(reg["bbox"], dst),
                  {"wall_s": 7200, "rss_gib": 100, "scratch_gib": 50})
    if rec["status"] != "ok":
        raise SystemExit(f"extract ended {rec['status']}")
    pin = {"file": name, "sha256": rm.sha256_file(dst), "bytes": dst.stat().st_size}
    MANIFEST.write_text(render_manifest(opts.region, reg["bbox"], pin))
    print(f"extract {opts.region}: {rec['wall_s']} s, {pin['bytes']:,} bytes; pinned in {MANIFEST}")


# --- collect ----------------------------------------------------------------


def ok_runs(results: Path, cell: str, binary: str) -> list[dict]:
    """The ok run rows of one cell and binary in a run_manifest results dir,
    with each run's fetched bytes and shim steps."""
    rows = [json.loads(p.read_text()) for p in sorted((results / cell).glob(f"{binary}-r*.json"))]
    for r in rows:
        stem = results / cell / f"{binary}-r{r['round']}"
        r["bytes_fetched"] = bytes_fetched(stem.with_suffix(".profile.jsonl"))
        r["shim"] = shim_phases(stem.with_suffix(".log"))
    return [r for r in rows if rm.run_status(r) == "ok"]


def median(xs: list[float]) -> float | None:
    """The median, or None for no values."""
    return statistics.median(xs) if xs else None


def tool_row(label: str, runs: list[dict], conversion_s: float | None = None,
             note: str | None = None, steps: dict | None = None, archive: dict | None = None) -> dict:
    """One row of gen_bench_docs.py's head-to-head schema, plus detail."""
    # The higher of the sampler's peak and GNU time's exact high-water mark:
    # the sampler can miss a spike between ticks.
    peak = [max(rm.true_peak(r) or 0, r["peak_rss_kib"] / 1024) * (1 << 20) for r in runs]
    cores = [r["cpu_s"] / r["wall_s"] for r in runs if r.get("cpu_s") and r["wall_s"]]
    fetched = [r["bytes_fetched"] for r in runs if r.get("bytes_fetched")]
    sizes = sorted({r["output"]["bytes"] for r in runs if r["output"]["bytes"]})
    shas = sorted({r["output"]["sha256"] for r in runs if r["output"]["sha256"]})
    # A kept archive describes this row only if the timed runs produced it.
    if archive and archive["sha256"] not in shas:
        archive = None
    return {"label": label, "runs_wall_s": [r["wall_s"] for r in runs],
            "conversion_wall_s": conversion_s,
            "peak_rss_bytes": int(max(peak)) if peak else None,
            "effective_cores": round(median(cores), 1) if cores else None,
            "archive_bytes": sizes[-1] if sizes else None,
            "archive_sha256": shas,
            "peak_disk_bytes": max(r["scratch_peak_mib"] for r in runs) * (1 << 20) if runs else None,
            "fetched_bytes": int(median(fetched)) if fetched else None,
            "steps": steps, "archive": archive, "note": note}


def archive_facts(path: Path, tylertoo: str, cap: int = 500_000) -> dict:
    """sha256 and `tylertoo stats --json` of a kept archive: per-zoom tile
    counts and sizes, and tiles over the cap among the 1000 largest."""
    stats = json.loads(subprocess.check_output(
        [tylertoo, "stats", "--json", "--largest", "1000", str(path)], text=True))
    return {"bytes": path.stat().st_size, "sha256": rm.sha256_file(path), "stats": stats,
            "tiles": sum(z["tile_count"] for z in stats["per_zoom"]),
            "max_tile_bytes": max((z["max_bytes"] for z in stats["per_zoom"]), default=None),
            "tiles_over_cap_in_1000_largest": sum(t["bytes"] > cap for t in stats["largest"])}


def build_doc(region: str, bbox: tuple, extract_rec: dict, extract_pin: dict, s3: list[dict],
              local: list[dict], tc: list[dict], parts: list[dict], prov: dict,
              archives: dict | None = None, bounded: list[dict] | None = None) -> dict:
    """The results document: gen_bench_docs.py's head-to-head schema, plus
    the per-step, disk, fetch and provenance detail of this showcase."""
    archives = archives or {}
    ext_s = extract_rec["wall_s"]
    convert = [r["shim"]["fgb_convert_s"] for r in tc if r.get("shim")]
    tile = [r["shim"]["tippecanoe_s"] for r in tc if r.get("shim")]
    touched = parts_touching(bbox, parts)
    feats = next((c["pass1_rows"] for r in local + s3
                  if (c := r["profile"].get("convert") or {}).get("pass1_rows")), None)
    tc_row = tool_row(f"tippecanoe {TIPPECANOE_VERSION}", tc,
                      conversion_s=ext_s + median(convert) if convert else None,
                      steps={"extract_s": ext_s, "fgb_convert_s": median(convert), "tile_s": median(tile)},
                      note="extract + FlatGeobuf + tile", archive=archives.get("tippecanoe"))
    # The shim's wall holds the conversion too; the row's runs are the tile step.
    tc_row["runs_wall_s"] = tile or tc_row["runs_wall_s"]
    rows = [
        tool_row("tylertoo, s3:// in place", s3, note="reads the parts in place; no extract",
                 archive=archives.get("tylertoo")),
        tool_row("tylertoo, local extract", local, conversion_s=ext_s,
                 note="end to end includes the gpio extract", archive=archives.get("tylertoo")),
        tc_row,
    ]
    if bounded:
        rows.insert(2, tool_row("tylertoo, local extract, `--profile bounded`", bounded,
                                conversion_s=ext_s, note="end to end includes the gpio extract",
                                archive=archives.get("tylertoo")))
    inputs = [{"name": f"overture-buildings-{RELEASE}", "release": RELEASE, "path": PREFIX,
               "bbox": list(bbox), "sha256": rm.sha256_file(PARTS_TSV), "sha256_of": PARTS_TSV.name,
               "parts": [{"url": p["url"], "etag": p["etag"], "bytes": p["bytes"]} for p in touched]},
              {"name": extract_pin["file"], "sha256": extract_pin["sha256"], "bytes": extract_pin["bytes"],
               "recipe": " ".join(extract_rec["argv"])}]
    return {"provenance": {**prov, "inputs": inputs},
            "dataset": f"Overture buildings {RELEASE}, {region} (bbox {bbox_arg(bbox)})",
            "features": feats, "input_bytes": extract_pin["bytes"],
            "region": region, "bbox": list(bbox),
            "parts_bytes": sum(p["bytes"] for p in touched),
            "extract": {"wall_s": ext_s, "peak_rss_bytes": (extract_rec["peak_rss_kib"] or 0) * 1024,
                        "peak_disk_bytes": extract_rec["scratch_peak_mib"] * (1 << 20)},
            "rows": rows}


def provenance(tylertoo: str) -> dict:
    """The contract's provenance fields for this host (inputs come later)."""
    def sh(cmd: list[str]) -> str | None:
        try:
            # tippecanoe prints its version on stderr
            return subprocess.check_output(cmd, text=True, stderr=subprocess.STDOUT).strip()
        except (OSError, subprocess.CalledProcessError):
            return None
    return {"commit": sh(["git", "-C", str(ROOT), "rev-parse", "HEAD"]),
            "tylertoo_version": sh([tylertoo, "--version"]),
            # No hostname: it is a private network address on cloud hosts.
            "host": "{machine}, {cpu_model}".format(**rm.host_class()),
            "nproc": os.cpu_count(), "kernel": platform.release(),
            "allocator": rm.detect_allocator(Path(tylertoo)),
            "tool_versions": {"tippecanoe": sh([TIPPECANOE, "--version"]),
                              "tippecanoe_commit": TIPPECANOE_SHA, "gpio": GPIO_VERSION},
            "inputs": [], "date": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}


def run_provenance(run_dir: Path, tylertoo: str) -> dict:
    """Provenance of the timed runs, not of the machine running collect: the
    run's saved provenance.json overrides this host's facts. The tool pins
    (tippecanoe commit, gpio version) stay the showcase's own."""
    prov = provenance(tylertoo)
    saved = json.loads((run_dir / "provenance.json").read_text())
    prov.update({k: saved[k] for k in ("commit", "tylertoo_version", "host", "nproc",
                                       "kernel", "allocator", "date") if k in saved})
    return prov


def collect(opts) -> None:
    """benchmarks/results/overture-buildings-<region>.json from the runs."""
    data = Path(opts.data).expanduser().resolve()
    reg = read_regions()[opts.region]
    pin = rm.load_manifest(MANIFEST)["inputs"][f"overture-{opts.region}"]
    ext = json.loads((data / "results" / f"{opts.region}-extract" / "extract.json").read_text())
    res = lambda s: data / "results" / f"{opts.region}-{s}"
    archives = {k: archive_facts(Path(v), opts.tylertoo)
                for k, v in (a.split("=", 1) for a in opts.archive)}
    doc = build_doc(opts.region, reg["bbox"], ext, pin, ok_runs(res("s3"), "ovt-s3", "tylertoo"),
                    ok_runs(res("local"), "ovt-local", "tylertoo"),
                    ok_runs(res("tc"), "ovt-local", "tippecanoe"), read_parts(),
                    run_provenance(res("s3"), opts.tylertoo), archives,
                    ok_runs(res("bounded"), "ovt-local-bounded", "tylertoo")
                    if res("bounded").is_dir() else None)
    out = Path(opts.out or RESULTS / f"overture-buildings-{opts.region}.json")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"wrote {out}")


def bbox_row_group_bytes(bbox: tuple, parts: list[dict]) -> tuple[int, int]:
    """Compressed bytes and rows of the row groups whose bbox statistics
    intersect `bbox`, over the touched parts (footers only, anonymous): the
    GeoParquet a --bbox run has in scope, every column counted."""
    from concurrent.futures import ThreadPoolExecutor

    import pyarrow.fs as pafs
    import pyarrow.parquet as pq

    s3 = pafs.S3FileSystem(anonymous=True, region="us-west-2")

    def one(url: str) -> tuple[int, int]:
        md = pq.ParquetFile(url.removeprefix("s3://"), filesystem=s3).metadata
        names = [md.schema.column(i).path for i in range(md.num_columns)]
        ix = [names.index(f"bbox.{k}") for k in ("xmin", "ymin", "xmax", "ymax")]
        b = n = 0
        for g in range(md.num_row_groups):
            rg = md.row_group(g)
            st = [rg.column(i).statistics for i in ix]
            if intersects((st[0].min, st[1].min, st[2].max, st[3].max), bbox):
                b += sum(rg.column(i).total_compressed_size for i in range(rg.num_columns))
                n += rg.num_rows
        return b, n

    with ThreadPoolExecutor(32) as ex:
        got = list(ex.map(one, [p["url"] for p in parts_touching(bbox, parts)]))
    return sum(b for b, _ in got), sum(n for _, n in got)


def build_stream_doc(region: str, bbox: tuple, s3: list[dict], parts: list[dict], prov: dict,
                     in_scope: tuple[int, int], archive: dict | None = None) -> dict:
    """The record of a streaming-only run: tylertoo reads the parts in place
    and no other tool takes part (tippecanoe cannot read s3:// GeoParquet)."""
    touched = parts_touching(bbox, parts)
    feats = next((c["pass1_rows"] for r in s3
                  if (c := r["profile"].get("convert") or {}).get("pass1_rows")), None)
    inputs = [{"name": f"overture-buildings-{RELEASE}", "release": RELEASE, "path": PREFIX,
               "bbox": list(bbox), "sha256": rm.sha256_file(PARTS_TSV), "sha256_of": PARTS_TSV.name,
               "parts": [{"url": p["url"], "etag": p["etag"], "bytes": p["bytes"]} for p in touched]}]
    # No other tool ran: drop the showcase's tippecanoe and gpio pins.
    return {"provenance": {**prov, "tool_versions": {}, "inputs": inputs},
            "dataset": f"Overture buildings {RELEASE}, {region} (bbox {bbox_arg(bbox)})",
            "features": feats, "region": region, "bbox": list(bbox),
            "release_bytes": sum(p["bytes"] for p in parts), "parts_touched": len(touched),
            "parts_bytes": sum(p["bytes"] for p in touched),
            "in_scope_bytes": in_scope[0], "in_scope_rows": in_scope[1],
            "rows": [tool_row("tylertoo, s3:// in place", s3, note="reads the parts in place; no extract",
                              archive=archive)]}


def collect_stream(opts) -> None:
    """benchmarks/results/overture-buildings-<region>.json for a streaming-only
    run (results dir <region>-s3, cell ovt-s3)."""
    data = Path(opts.data).expanduser().resolve()
    bbox = read_regions()[opts.region]["bbox"]
    run_dir = data / "results" / f"{opts.region}-s3"
    archive = archive_facts(Path(opts.archive), opts.tylertoo) if opts.archive else None
    parts = read_parts()
    doc = build_stream_doc(opts.region, bbox, ok_runs(run_dir, "ovt-s3", "tylertoo"), parts,
                           run_provenance(run_dir, opts.tylertoo),
                           bbox_row_group_bytes(bbox, parts), archive)
    out = Path(opts.out or RESULTS / f"overture-buildings-{opts.region}.json")
    out.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"wrote {out}")


# --- CLI --------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    """Subcommands: regions, calibrate, extract, collect, collect-stream."""
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("regions")
    c = sub.add_parser("calibrate")
    c.add_argument("--tylertoo", required=True)
    c.add_argument("--regions", default=f"smoke,{CALIBRATION}",
                   help="the calibration bboxes: a tiny one for the fixed costs, then 1-3M buildings")
    c.add_argument("--work", default="~/bench-data/showcase/calibrate")
    c.add_argument("--rss-gib", type=float, default=100)
    e = sub.add_parser("extract")
    e.add_argument("region")
    e.add_argument("--data", default="~/bench-data/showcase")
    k = sub.add_parser("collect")
    k.add_argument("region")
    k.add_argument("--tylertoo", required=True)
    k.add_argument("--data", default="~/bench-data/showcase")
    k.add_argument("--archive", action="append", default=[], metavar="TOOL=PATH",
                   help="a kept archive (tylertoo=..., tippecanoe=...) for `stats --json`")
    k.add_argument("--out")
    k = sub.add_parser("collect-stream")
    k.add_argument("region")
    k.add_argument("--tylertoo", required=True)
    k.add_argument("--data", default="~/bench-data/showcase")
    k.add_argument("--archive", help="the kept archive of the timed run")
    k.add_argument("--out")
    a = ap.parse_args(argv)
    {"regions": regions, "calibrate": calibrate, "extract": extract, "collect": collect,
     "collect-stream": collect_stream}[a.cmd](a)
    return 0


if __name__ == "__main__":
    sys.exit(main())
