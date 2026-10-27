#!/usr/bin/env python3
"""
run_e2e.py — end-to-end GeoParquet → PMTiles benchmark: tylertoo vs a pinned
tippecanoe (issue #447).

WHAT THIS MEASURES, AND WHY IT IS SHAPED THIS WAY
-------------------------------------------------
tippecanoe cannot read GeoParquet, and at defaults the two tools do not emit
the same map. Any "tylertoo vs tippecanoe" number is therefore two choices —
what to put on tippecanoe's side of the scale, and whether to hold output
constant — and those choices are where benchmarks lie. This harness refuses to
make them silently: it measures four numbers per dataset and prints them all.

  A.  tylertoo e2e      parquet  → pmtiles          (defaults, one process)
  A'. tylertoo matched  parquet  → pmtiles          (thinning ladder off, so
                                                     the same features reach
                                                     every zoom; --quality-matched)
  B.  tippecanoe e2e    parquet  → fgb → pmtiles    (convert + tile)
  C.  tippecanoe tiles  fgb      → pmtiles          (tile only, input already
                                                     in tippecanoe's best format)

A vs B is the honest product comparison: it is what a user with a GeoParquet
file actually pays, and tylertoo's native columnar read is a real part of the
product, not a benchmarking trick.

A vs C is the deliberately unflattering comparison: it hands tippecanoe a
pre-converted FlatGeobuf for free and asks whether the tiler itself is faster.
Report both or report neither.

A' vs C is the comparison to quote to a skeptic. tylertoo's density budget
thins features hard at coarse and mid zooms. tippecanoe's rate-based dropping
applies only to points at defaults; it still removes small polygons at low
zooms (tiny-polygon reduction, which -pt disables), but on a contiguous
polygon coverage it keeps far more of them and reaches every feature by a
mid zoom (all 17,465 of madagascar-adm4 by z8, against tylertoo's 865).
Measuring A against C on such a dataset compares against less work. A' turns
the ladder off (keeping simplification) and reaches near-parity: tile counts
within 2 per zoom, features within 0.4% at z8 and ~5% at z4.

The conversion step (B − C) is printed on its own so a reader can see exactly
how much of the e2e gap is input format. A line-delimited GeoJSON variant is
also measured (`--geojson`) because "fgb is tippecanoe's best input" is a
claim that should carry evidence, and because ldGeoJSON + `-P` is what most
real GeoParquet→tippecanoe pipelines actually run.

FAIRNESS RULES THIS SCRIPT FOLLOWS
----------------------------------
* tippecanoe is pinned (see setup_tippecanoe.sh). Its `--version` must equal
  EXPECTED_TIPPECANOE_VERSION on every run, and goes into every result file
  with the build tag + SHA when the binary is the one setup_tippecanoe.sh
  built (null otherwise). Never "whatever is on PATH".
* Both tools get the same zoom range, the same layer name, the same tile
  buffer and the same per-tile byte cap. The exact argv of both is recorded.
* A discarded warm-up run precedes the timed repeats, so every measurement is
  warm-cache for both tools. Median of N (default 3) is reported, with min/max.
* Peak RSS comes from /usr/bin/time (Darwin `-l` / GNU `-v`), not sampling.
* Asymmetries that cannot be closed by flags are listed in README.md and are
  NOT quietly tuned away in either direction.

USAGE
-----
    ./setup_tippecanoe.sh
    cargo build --release -p tylertoo
    python3 run_e2e.py --repeat 3 --geojson --quality-matched   # the full run
        # -> results-<UTC timestamp>.json (gitignored); pass --out results.json
        #    only when deliberately replacing the committed record
    python3 run_e2e.py --only madagascar-adm4 --repeat 5
    python3 run_e2e.py --dataset big=/data/planet-buildings.parquet \
                       --only big --max-zoom 12         # cluster-scale run

Only the standard library is used, so it runs anywhere the two binaries do.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

# --------------------------------------------------------------------------
# Datasets
#
# The committed set is the in-repo real-data fixture set: small enough that
# `git clone && run` reproduces the table, real enough that the geometry
# classes (polygon / multipolygon / line) behave like production data.
#
# Two of the three are ~1k features. At that size BOTH tools are dominated by
# process start-up and pyramid scaffolding, and the ratio says almost nothing
# about tiling throughput. They are kept because they are the geometry-class
# smoke test, and RESULTS.md says plainly that they are not the headline.
# The headline dataset is madagascar-adm4 (17,465 multipolygons, 28 MB).
#
# For cluster-scale runs pass --dataset id=/path/to.parquet; everything else
# (flags, parity, reporting) is identical.
# --------------------------------------------------------------------------
FIXTURES = REPO / "tests" / "fixtures" / "realdata"

# The corpus tier (--corpus): the datasets corpus/manifest.json defines, read
# from the gpio-optimized copies (Hilbert-sorted, bbox-covered GeoParquet 1.1
# — the input contract tylertoo documents). These are the engine-scale rows
# (280k–650k features) the fixture set lacks; build them with
# `corpus/fetch.sh && corpus/optimize.sh` (~2–3 GB download).
#
# On the tippecanoe side each corpus dataset carries the manifest's RECORDED
# tippecanoe flags (the same flags corpus/goldens.sh uses), appended to the
# settings-parity set and written into the results file. tylertoo runs its own
# defaults against them: both tools in their production configuration for that
# data, rather than both at bare defaults — a 650k-building z14 run without
# density dropping is a configuration nobody ships. The per-zoom tile counts
# are recorded for both archives either way, so the output difference is on
# the record next to the timing.
CORPUS_MANIFEST = REPO / "corpus" / "manifest.json"
CORPUS_GPIO = REPO / "corpus" / "data" / "gpio"

DATASETS = {
    "madagascar-adm4": {
        "path": FIXTURES / "fieldmaps-madagascar-adm4.parquet",
        "layer": "adm4",
        "note": "17,465 MultiPolygon admin-4 boundaries, 28 MB (FieldMaps)",
    },
    "open-buildings": {
        "path": FIXTURES / "open-buildings.parquet",
        "layer": "buildings",
        "note": "1,000 Polygon building footprints, 143 KB (Google Open Buildings)",
    },
    "road-detections": {
        "path": FIXTURES / "road-detections.parquet",
        "layer": "roads",
        "note": "1,000 LineString road detections, 90 KB",
    },
    # Dense small fields: the data behind #748 (invalid polygons at coarse
    # zooms), #750 and #762 (coverage at coarse zooms). No properties.
    "ftw-moldova-z10": {
        "path": FIXTURES / "ftw-moldova-z10.parquet",
        "layer": "fields",
        "note": "10,931 Polygon field boundaries, one z10 tile of FTW Moldova, 1.9 MB",
    },
    # Three huge admin-1 polygons: few features, many vertices, a tile
    # split at every zoom.
    "fieldmaps-boundaries": {
        "path": FIXTURES / "fieldmaps-boundaries.parquet",
        "layer": "adm1",
        "note": "3 MultiPolygon admin-1 boundaries, 2.2 MB (FieldMaps)",
    },
}


def corpus_datasets() -> dict:
    """Dataset entries for every corpus/manifest.json dataset (--corpus).

    The path is the gpio-optimized copy; a dataset whose copy has not been
    built is skipped at run time with the fetch recipe, not an error, so
    `--corpus` works with whatever subset of the corpus is on disk.
    """
    if not CORPUS_MANIFEST.exists():
        raise FileNotFoundError(f"corpus manifest not found: {CORPUS_MANIFEST}")
    manifest = json.loads(CORPUS_MANIFEST.read_text())
    out = {}
    for ds in manifest["datasets"]:
        did = ds["id"]
        tc = ds.get("tippecanoe", {})
        feats = ds.get("expected_features")
        feats_s = f"~{feats:,} features" if isinstance(feats, int) else "features: ?"
        out[did] = {
            "path": CORPUS_GPIO / f"{did}.parquet",
            "layer": did,
            "note": f"corpus {ds.get('class')}/{ds.get('tier')}: "
                    f"{ds.get('region')} ({feats_s})",
            "min_zoom": tc.get("minzoom"),
            "max_zoom": tc.get("maxzoom"),
            # The manifest's recorded tippecanoe flags for this dataset —
            # the same ones corpus/goldens.sh passes — appended to the
            # parity set and recorded in the results file.
            "tippecanoe_flags": tc.get("flags", "").split(),
            "fetch_hint": "cd corpus && ./fetch.sh && ./optimize.sh",
        }
    return out

# --------------------------------------------------------------------------
# Settings parity
#
# These are tylertoo's `tiles` defaults, mapped onto the nearest tippecanoe
# flag, and passed EXPLICITLY on both sides so neither tool's default decides.
# The per-tile cap: tippecanoe's --maximum-tile-bytes caps the
# gzip-compressed tile at 500,000 by default. tylertoo's default (#761) caps
# the compressed tile at `500K` = 512,000 bytes and the raw tile at 2M, so
# the harness passes `--max-compressed-tile-size 500000 --max-tile-size 0`
# to tylertoo: the same compressed cap, and no raw cap, as tippecanoe has
# none. Where tylertoo's default
# differs from tippecanoe's
# (buffer 8 vs 5) the tippecanoe side is moved to tylertoo's value rather than
# the reverse, because the buffer changes output *size*, and letting
# tippecanoe write smaller tiles from a smaller buffer would flatter tylertoo
# on the archive-size column.
#
# --drop-fraction-as-needed is the closest analogue of tylertoo's per-tile
# size valve. tippecanoe's default when a tile blows the byte budget is to
# give up on that tile; without this flag the comparison would be against an
# archive with holes in it. It is an IMPERFECT match (tippecanoe's loop is
# iterative re-encode, tylertoo's is one non-iterative pass) and README.md
# says so.
#
# Deliberately NOT passed: -r / -g / -S overrides. tylertoo's --drop-rate
# 1.65 and --drop-gamma 1.5 are anchored on a full-canonical-count budget,
# not tippecanoe's per-tile basezoom count; hand-matching the numerals would
# be cargo-culting, not parity. Both tools run their own defaults and the
# asymmetry is documented instead.
# --------------------------------------------------------------------------
DEFAULT_MIN_ZOOM = 0
DEFAULT_MAX_ZOOM = 14
TILE_BUFFER = 8            # tylertoo default; tippecanoe default is 5
# tippecanoe's default cap on the compressed tile. NOT tylertoo's default
# (its `500K` is 512,000 bytes), so the harness passes 500000 to both tools
# explicitly.
MAX_TILE_BYTES = 500_000

# The pinned baseline, exactly as `tippecanoe --version` prints it. Enforced on
# every run, whether or not the gitignored, machine-local tippecanoe.lock.json
# exists: a missing lockfile must never mean "any binary will do". Keep in
# step with TIPPECANOE_TAG in setup_tippecanoe.sh.
EXPECTED_TIPPECANOE_VERSION = "tippecanoe v2.79.0"


# --------------------------------------------------------------------------
# measurement
# --------------------------------------------------------------------------

def _time_flag() -> str:
    return "-l" if platform.system() == "Darwin" else "-v"


def _parse_peak_rss(text: str) -> int | None:
    """Peak RSS in bytes from /usr/bin/time output (GNU or BSD/Darwin)."""
    m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", text)
    if m:
        return int(m.group(1)) * 1024
    # Darwin: "  123456789  maximum resident set size" (already bytes)
    m = re.search(r"^\s*(\d+)\s+maximum resident set size", text, re.M)
    if m:
        return int(m.group(1))
    return None


def _parse_cpu_pct(text: str) -> float | None:
    """CPU utilisation (%) from /usr/bin/time output (GNU or BSD/Darwin).

    GNU time prints it directly; Darwin prints real/user/sys on one line and
    the percentage is (user + sys) / real. >100% means multi-core."""
    m = re.search(r"Percent of CPU this job got:\s*(\d+)\s*%", text)
    if m:
        return float(m.group(1))
    m = re.search(r"^\s*([\d.]+)\s+real\s+([\d.]+)\s+user\s+([\d.]+)\s+sys",
                  text, re.M)
    if m:
        real, user, sys_ = (float(g) for g in m.groups())
        if real > 0:
            return round(100.0 * (user + sys_) / real, 1)
    return None


class CommandFailed(RuntimeError):
    def __init__(self, argv, rc, tail):
        super().__init__(f"exit {rc}: {' '.join(argv)}")
        self.argv, self.rc, self.tail = argv, rc, tail


def measure(argv: list[str], *, unlink_first: Path | None = None) -> dict:
    """Run argv once under /usr/bin/time; return wall seconds + peak RSS.

    Wall comes from perf_counter (identical semantics on every platform);
    peak RSS comes from the kernel via /usr/bin/time, which is exact rather
    than sampled. The command's own stderr is interleaved with time's report
    in the capture buffer; the regexes tolerate that.
    """
    # ogr2ogr's FlatGeobuf driver cannot DeleteLayer(), so `-overwrite` fails
    # on the second run. Removing the target first is also what makes every
    # repeat measure the same work rather than an overwrite fast path.
    if unlink_first is not None:
        unlink_first.unlink(missing_ok=True)
    wrapped = ["/usr/bin/time", _time_flag(), *argv]
    with tempfile.TemporaryFile("w+") as err:
        t0 = time.perf_counter()
        rc = subprocess.call(wrapped, stdout=subprocess.DEVNULL, stderr=err)
        wall = time.perf_counter() - t0
        err.seek(0)
        errtext = err.read()
    if rc != 0:
        raise CommandFailed(argv, rc, errtext[-2000:])
    return {
        "wall_s": wall,
        "peak_rss_bytes": _parse_peak_rss(errtext),
        "cpu_pct": _parse_cpu_pct(errtext),
        "argv": argv,
    }


def repeat(argv: list[str], n: int, *, warmup: bool = True,
           unlink_first: Path | None = None) -> dict:
    """Warm-up run (discarded) then n timed runs; median reported."""
    if warmup:
        measure(argv, unlink_first=unlink_first)
    runs = [measure(argv, unlink_first=unlink_first) for _ in range(n)]
    walls = [r["wall_s"] for r in runs]
    rss = [r["peak_rss_bytes"] for r in runs if r["peak_rss_bytes"]]
    cpu = [r["cpu_pct"] for r in runs if r["cpu_pct"] is not None]
    return {
        "argv": argv,
        "runs": n,
        "wall_median_s": statistics.median(walls),
        "wall_min_s": min(walls),
        "wall_max_s": max(walls),
        "peak_rss_bytes": max(rss) if rss else None,
        # Median utilisation across the repeats; >100% means multi-core.
        "cpu_pct": round(statistics.median(cpu), 1) if cpu else None,
        "wall_all_s": walls,
    }


# --------------------------------------------------------------------------
# archive inspection
# --------------------------------------------------------------------------

def per_zoom_counts(tylertoo: str, archive: Path) -> dict:
    """Per-zoom tile counts and stored bytes, via `tylertoo stats --json`.

    Works on any PMTiles archive, including tippecanoe's, so both sides are
    measured by the same code reading the same directory entries.
    """
    try:
        raw = subprocess.check_output(
            [tylertoo, "stats", str(archive), "--json", "--largest", "0"],
            stderr=subprocess.DEVNULL,
        )
    except (subprocess.CalledProcessError, OSError) as exc:
        return {"error": str(exc)}
    try:
        doc = json.loads(raw)
    except json.JSONDecodeError:
        return {"error": "stats did not return JSON"}
    out = {}
    for z in doc.get("per_zoom", []):
        out[str(z["z"])] = {
            "tiles": z.get("tile_count"),
            "total_bytes": z.get("total_bytes"),
            "max_bytes": z.get("max_bytes"),
        }
    return {"by_zoom": out,
            "tiles_total": sum(v["tiles"] or 0 for v in out.values())}


# --------------------------------------------------------------------------
# input conversion (tippecanoe's side of the input-parity problem)
# --------------------------------------------------------------------------

def fgb_converters(src: Path, dstdir: Path, stem: str) -> dict[str, tuple[list[str], Path]]:
    """Every available parquet → FlatGeobuf converter, so the baseline gets its
    best shot.

    gpio is the tool this project recommends for GeoParquet preprocessing, but
    it is a Python CLI and pays ~0.3 s of interpreter start-up per invocation.
    On a 1,000-feature fixture that start-up IS the conversion time, and using
    gpio alone would hand tylertoo a free win that has nothing to do with
    tiling. ogr2ogr is a C binary with negligible start-up. The harness times
    both when both exist and the e2e number uses the FASTER one — the
    opposite of cherry-picking.
    """
    out = {}
    if shutil.which("ogr2ogr"):
        dst = dstdir / f"{stem}.ogr.fgb"
        out["ogr2ogr"] = (["ogr2ogr", "-f", "FlatGeobuf", str(dst), str(src)],
                          dst)
    if shutil.which("gpio"):
        dst = dstdir / f"{stem}.gpio.fgb"
        out["gpio"] = (["gpio", "convert", "flatgeobuf", str(src), str(dst),
                        "--overwrite"], dst)
    return out


def geojsonseq_argv(src: Path, dst: Path) -> list[str] | None:
    """parquet → line-delimited GeoJSON (tippecanoe's `-P` parallel input)."""
    if shutil.which("ogr2ogr"):
        return ["ogr2ogr", "-f", "GeoJSONSeq", str(dst), str(src)]
    return None


# --------------------------------------------------------------------------
# the run
# --------------------------------------------------------------------------

# Flags for the QUALITY-MATCHED tylertoo run (see --quality-matched).
#
# At defaults the two tools do not emit the same map. tylertoo's density budget
# thins features hard at coarse and mid zooms. tippecanoe's rate-based
# dropping applies only to points at defaults; its tiny-polygon reduction
# (disabled by -pt) still removes small polygons at low zooms, but for a
# contiguous polygon coverage (admin boundaries, parcels) it keeps most of
# them and reaches every feature by a mid zoom, while tylertoo emits a sample.
# On the madagascar fixture tippecanoe has all 17,465 polygons by z8 and
# tylertoo's default has 865 — a visibly different map, and comparing wall
# time against it would be comparing against less work.
#
# `--verbatim` switches the whole thinning/visibility ladder off, and
# `--simplify-factor` is then set back explicitly (verbatim would otherwise
# disable simplification too, and tippecanoe does simplify). The tile cap
# likewise has to be restated because verbatim disables the caps (so the size
# valve still fires). The result is near-parity
# with tippecanoe, not identity: tile counts within 2 per zoom, features
# within 0.4% at z8 and ~5% at z4.
QUALITY_MATCHED_FLAGS = ["--verbatim", "--simplify-factor", "1.0"]


def tylertoo_argv(binary, src, dst, layer, zmin, zmax, report, extra=()):
    return [
        binary, "tiles", str(src), str(dst),
        "--min-zoom", str(zmin),
        "--max-zoom", str(zmax),
        "--layer-name", layer,
        "--tile-buffer", str(TILE_BUFFER),
        "--max-compressed-tile-size", str(MAX_TILE_BYTES),
        "--max-tile-size", "0",
        "--report", str(report),
        "--force",
        *extra,
    ]


def tippecanoe_argv(binary, src, dst, layer, zmin, zmax, parallel, extra):
    argv = [
        binary,
        "-o", str(dst), "-f",
        "-Z", str(zmin), "-z", str(zmax),
        "-l", layer,
        "-b", str(TILE_BUFFER),
        "--maximum-tile-bytes", str(MAX_TILE_BYTES),
        "--drop-fraction-as-needed",
        "--quiet",
    ]
    if parallel:
        argv.append("-P")   # parallel input parsing; ldGeoJSON only
    argv += extra
    argv.append(str(src))
    return argv


def machine_info() -> dict:
    def sh(cmd):
        try:
            return subprocess.check_output(cmd, shell=True, text=True,
                                           stderr=subprocess.DEVNULL).strip()
        except Exception:
            return None
    info = {
        "platform": platform.platform(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
    }
    if platform.system() == "Darwin":
        info["cpu"] = sh("sysctl -n machdep.cpu.brand_string")
        mem = sh("sysctl -n hw.memsize")
        info["ram_gb"] = round(int(mem) / 1024**3) if mem else None
    else:
        info["cpu"] = sh("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-")
        mem = sh("grep MemTotal /proc/meminfo | awk '{print $2}'")
        info["ram_gb"] = round(int(mem) / 1024**2) if mem else None
    return info


# --------------------------------------------------------------------------
# --invariants: whole-archive invariants under a settings matrix (#779, G3)
#
# No tippecanoe here. Each (dataset, settings) cell runs `tylertoo tiles` on
# a copy of the dataset with one extra column, `g3_fid` (the row index),
# passed as `--feature-id`, so every MVT feature carries a source-unique id
# and scripts/verify_archive.py --strict can count distinct features per
# zoom. verify_archive then decodes EVERY tile and checks tile size, polygon
# validity, feature and property accounting, monotone counts and the decay
# of placeholder squares; known failures live in invariant_exemptions.toml,
# one row per {fixture, settings, zoom, invariant, issue}.
#
# The settings are the ones users run that the structural guard
# (ci_guard.py, defaults + --mode duplicating) never does: #749 needs
# --polygon-visibility 0, #758 --simplify-factor 0, #750/#762 the coverage
# fill. `--tier pr` runs defaults plus each dataset's most bug-prone setting;
# `--tier full` the whole matrix (push to main, nightly).
# --------------------------------------------------------------------------
INVARIANT_ID_COLUMN = "g3_fid"
EXEMPTIONS = HERE / "invariant_exemptions.toml"
VERIFY_ARCHIVE = REPO / "scripts" / "verify_archive.py"

# name -> (tiles flags, caps the archive is built with as verify_archive
# flags, whether placeholder squares must decay toward coarse zooms).
#
# Decay is checked only with the polygon gate at 0, where every small polygon
# reaches the coarse levels as a dithered square, so the squares must thin
# ~4x per zoom (#749). With the gate on, small polygons leave at assignment
# and the #384 accumulator draws carrier squares at a density-budget rate,
# which need not halve over three zooms; and the coverage fill draws
# square child-tile cells by design.
_DEFAULT_CAPS: list[str] = []
_NO_CAPS = ["--max-tile-size", "0", "--max-compressed-tile-size", "0"]
INVARIANT_SETTINGS = {
    "defaults": ([], _DEFAULT_CAPS, False),
    # --verbatim turns thinning, simplification and both caps off.
    "verbatim": (["--verbatim"], _NO_CAPS, False),
    "pv0": (["--polygon-visibility", "0"], _DEFAULT_CAPS, True),
    "sf0": (["--simplify-factor", "0"], _DEFAULT_CAPS, False),
    "fill": (["--coverage-fill"], _DEFAULT_CAPS, False),
}
# Datasets only the invariant matrix runs (no tippecanoe benchmark value):
# committed fixtures under crates/core/tests/fixtures/invariants/, from
# make_fixtures.py there.
INVARIANT_FIXTURES = REPO / "crates" / "core" / "tests" / "fixtures" / "invariants"
INVARIANT_ONLY_DATASETS = {
    # One NaN and one +inf vertex mid-ring: export drops both features with
    # no counter (#707), which feature_accounting sees.
    "nonfinite-vertex": {
        "path": INVARIANT_FIXTURES / "nonfinite-vertex.parquet",
        "layer": "buildings",
        "note": "200 open-buildings rows, two with a non-finite vertex",
    },
}
# --tier pr: defaults plus the setting most likely to break, per dataset.
INVARIANT_PR_TIER = {
    "open-buildings": ["defaults", "pv0"],  # #749: squares survive the cascade
    "road-detections": ["defaults", "sf0"],  # lines only; #758 ran sf0
    "madagascar-adm4": ["defaults", "fill"],  # #750 coverage fill
    "ftw-moldova-z10": ["defaults", "fill"],  # #748 validity, #750/#762 fill
    "fieldmaps-boundaries": ["defaults", "verbatim"],  # uncapped huge rings
    "nonfinite-vertex": ["defaults"],  # #707 silent drop
}


def _grouped(outcomes: list[dict], status: str) -> list[str]:
    """`invariant (z4,z5) #748` per invariant with `status`."""
    by: dict[tuple, list[str]] = {}
    for o in outcomes:
        if o["status"] == status:
            by.setdefault((o["invariant"], o["issue"]), []).append(o["zoom"])
    out = []
    for (inv, issue), zooms in sorted(by.items(), key=lambda kv: kv[0][0]):
        zs = sorted(zooms, key=lambda z: -1 if z == "*" else int(z[1:]))
        where = "" if zs == ["*"] else f" ({','.join(zs)})"
        out.append(f"{inv}{where}" + (f" #{issue}" if issue else ""))
    return out


def run_invariants(args, datasets: dict) -> int:
    """The settings matrix over `datasets`; returns the exit status."""
    tylertoo = args.tylertoo
    if not Path(tylertoo).exists():
        return fail(f"tylertoo binary not found: {tylertoo}\n"
                    f"  build it: cargo build --release -p tylertoo")
    uv = shutil.which("uv")
    if uv is None:
        return fail("uv not found: verify_archive.py runs under `uv run`")
    settings_filter = set(args.settings or [])
    unknown = settings_filter - set(INVARIANT_SETTINGS)
    if unknown:
        return fail(f"unknown --settings {sorted(unknown)}; "
                    f"known: {sorted(INVARIANT_SETTINGS)}")
    cells = []
    for did in datasets:
        # The invariant-only fixtures each target one bug under one setting.
        full = args.tier == "full" and did not in INVARIANT_ONLY_DATASETS
        names = (list(INVARIANT_SETTINGS) if full
                 else INVARIANT_PR_TIER.get(did, ["defaults"]))
        cells += [(did, s) for s in names
                  if not settings_filter or s in settings_filter]
    if not cells:
        return fail("no (dataset, settings) cell selected")

    workdir = Path(args.work) if args.work else Path(tempfile.mkdtemp(prefix="tt-inv-"))
    workdir.mkdir(parents=True, exist_ok=True)
    pmtiles_bin = os.environ.get("PMTILES_BIN")
    rows, outcomes, failed = [], [], []
    t_all = time.monotonic()
    try:
        prepared: dict[str, Path] = {}
        for did, sname in cells:
            ds = datasets[did]
            src = Path(ds["path"])
            if not src.exists():
                return fail(f"{did}: missing fixture {src}")
            if did not in prepared:
                dst = workdir / f"{did}.ids.parquet"
                subprocess.run(
                    [uv, "run", "-q", str(VERIFY_ARCHIVE), "--add-feature-ids",
                     str(src), str(dst), "--feature-id", INVARIANT_ID_COLUMN],
                    check=True, stdout=subprocess.DEVNULL)
                prepared[did] = dst
            flags, caps, decay = INVARIANT_SETTINGS[sname]
            tag = f"{did}.{sname}"
            archive = workdir / f"{tag}.pmtiles"
            report = workdir / f"{tag}.report.json"
            argv = [tylertoo, "tiles", str(prepared[did]), str(archive),
                    "--min-zoom", str(args.min_zoom), "--max-zoom", str(args.max_zoom),
                    "--layer-name", ds["layer"], "--feature-id", INVARIANT_ID_COLUMN,
                    "--report", str(report), "--force", "-q", *flags]
            print(f"\n=== {tag}: {' '.join(argv[1:])}", flush=True)
            t0 = time.monotonic()
            proc = subprocess.run(argv, capture_output=True, text=True)
            t_tiles = time.monotonic() - t0
            if proc.returncode != 0:
                print(proc.stderr[-4000:])
                failed.append(f"{tag}: tiles exited {proc.returncode}")
                rows.append((did, sname, t_tiles, 0.0, "tiles failed"))
                continue
            if pmtiles_bin:
                pv = subprocess.run([pmtiles_bin, "verify", str(archive)],
                                    capture_output=True, text=True)
                if pv.returncode != 0:
                    print(pv.stdout[-2000:], pv.stderr[-2000:])
                    failed.append(f"{tag}: pmtiles verify failed")
            out_json = workdir / f"{tag}.outcomes.json"
            vargv = [uv, "run", "-q", str(VERIFY_ARCHIVE), str(archive),
                     "--layer", ds["layer"], "--strict", "--report", str(report),
                     "--input", str(prepared[did]), "--ids",
                     "--feature-id", INVARIANT_ID_COLUMN, *caps,
                     "--exemptions", str(args.exemptions),
                     "--fixture", did, "--settings", sname,
                     "--json-out", str(out_json)]
            if decay:
                vargv.append("--expect-decay")
            t1 = time.monotonic()
            vp = subprocess.run(vargv)
            t_verify = time.monotonic() - t1
            cell = json.loads(out_json.read_text()) if out_json.exists() else []
            outcomes += cell
            if vp.returncode != 0:
                failed.append(f"{tag}: strict invariants failed")
            summary = "; ".join(
                [f"{st} {g}" for st in ("FAIL", "XPASS")
                 for g in _grouped(cell, st)]
                + [f"xfail {g}" for g in _grouped(cell, "XFAIL")]) or "all pass"
            if vp.returncode != 0 and not any(o["status"] in ("FAIL", "XPASS") for o in cell):
                summary = "verify_archive failed (see log)"
            rows.append((did, sname, t_tiles, t_verify, summary))
            if not args.keep:
                archive.unlink(missing_ok=True)
    finally:
        if not args.keep and not args.work:
            shutil.rmtree(workdir, ignore_errors=True)
    wall = time.monotonic() - t_all

    table = ["| fixture | settings | tiles s | verify s | result |",
             "|---|---|--:|--:|---|"]
    table += [f"| {d} | {s} | {a:.1f} | {b:.1f} | {r} |" for d, s, a, b, r in rows]
    table.append(f"\n{len(rows)} cell(s), {wall:.0f} s wall")
    print("\n" + "\n".join(table))
    if args.markdown:
        Path(args.markdown).write_text("\n".join(table) + "\n")
    if args.invariants_out:
        Path(args.invariants_out).write_text(json.dumps(outcomes, indent=1) + "\n")
    if failed:
        print(f"\n{len(failed)} cell(s) FAILED:")
        for f in failed:
            print(f"  - {f}")
        return 1
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repeat", type=int, default=3,
                    help="timed runs per measurement (median reported); default 3")
    ap.add_argument("--only", action="append", default=None,
                    help="restrict to these dataset ids (repeatable)")
    ap.add_argument("--dataset", action="append", default=[],
                    metavar="ID=PATH",
                    help="register an extra GeoParquet dataset (cluster-scale runs)")
    ap.add_argument("--corpus", action="store_true",
                    help="also register every corpus/manifest.json dataset "
                         "(gpio-optimized copies; build them with "
                         "corpus/fetch.sh && corpus/optimize.sh). Each carries "
                         "the manifest's recorded tippecanoe flags and zoom "
                         "range; datasets not on disk are skipped")
    ap.add_argument("--min-zoom", type=int, default=DEFAULT_MIN_ZOOM)
    ap.add_argument("--max-zoom", type=int, default=DEFAULT_MAX_ZOOM)
    ap.add_argument("--tylertoo", default=os.environ.get(
        "TYLERTOO", str(REPO / "target" / "release" / "tylertoo")))
    ap.add_argument("--tippecanoe", default=os.environ.get("TIPPECANOE"),
                    help="default: the binary named in tippecanoe.lock.json. "
                         "Its --version must be "
                         f"{EXPECTED_TIPPECANOE_VERSION!r} either way")
    ap.add_argument("--tippecanoe-extra", default="",
                    help="extra tippecanoe flags, space-separated, appended to "
                         "the parity set and recorded in the results file "
                         "(e.g. --tippecanoe-extra=-pf). Pass it with '=' or "
                         "argparse will swallow the leading dash. For probing "
                         "how much a single flag costs — publish the parity "
                         "set, not a variant, unless you say which")
    ap.add_argument("--quality-matched", action="store_true",
                    help="also run tylertoo with the thinning ladder off "
                         f"({' '.join(QUALITY_MATCHED_FLAGS)}) so its per-zoom "
                         "output is near tippecanoe's for polygons/lines. "
                         "This is the comparison to quote when output parity "
                         "matters more than defaults")
    ap.add_argument("--geojson", action="store_true",
                    help="also measure the ldGeoJSON + -P tippecanoe path")
    ap.add_argument("--work", default=None,
                    help="working directory for intermediates (default: a temp dir)")
    ap.add_argument("--keep", action="store_true",
                    help="keep intermediates and output archives")
    ap.add_argument("--out", default=None,
                    help="results file (default: results-<UTC timestamp>.json "
                         "next to this script, gitignored). The committed "
                         "record results.json is only ever written when named "
                         "here explicitly")
    ap.add_argument("--markdown", default=None,
                    help="also write a markdown table here")
    inv = ap.add_argument_group(
        "whole-archive invariants (#779): no tippecanoe, no timing repeats")
    inv.add_argument("--invariants", action="store_true",
                     help="run the strict invariant matrix instead of the "
                          "benchmark: tiles + verify_archive.py --strict per "
                          "(dataset, settings) cell")
    inv.add_argument("--tier", choices=("pr", "full"), default="full",
                     help="pr: defaults + one bug-prone setting per dataset; "
                          "full: every setting (default)")
    inv.add_argument("--settings", action="append", default=None,
                     help=f"restrict to these settings ({', '.join(INVARIANT_SETTINGS)})")
    inv.add_argument("--exemptions", default=str(EXEMPTIONS),
                     help="the exemption file (default: invariant_exemptions.toml)")
    inv.add_argument("--invariants-out", default=None,
                     help="write every invariant outcome here as JSON")
    args = ap.parse_args()
    if args.invariants:
        datasets = {**DATASETS, **INVARIANT_ONLY_DATASETS}
        for spec in args.dataset:
            if "=" not in spec:
                return fail(f"--dataset expects ID=PATH, got {spec!r}")
            did, dpath = spec.split("=", 1)
            datasets[did] = {"path": Path(dpath), "layer": did}
        if args.only:
            missing = [d for d in args.only if d not in datasets]
            if missing:
                return fail(f"unknown dataset(s): {', '.join(missing)}")
            datasets = {k: v for k, v in datasets.items() if k in args.only}
        return run_invariants(args, datasets)
    if args.out is None:
        stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        args.out = str(HERE / f"results-{stamp}.json")

    # --- resolve binaries -------------------------------------------------
    tylertoo = args.tylertoo
    if not Path(tylertoo).exists():
        return fail(f"tylertoo binary not found: {tylertoo}\n"
                    f"  build it: cargo build --release -p tylertoo")

    try:
        tip = resolve_tippecanoe(args.tippecanoe)
    except TippecanoeUnavailable as exc:
        return fail(str(exc))
    tippecanoe, tip_version = tip["binary"], tip["version"]
    tip_tag, tip_sha = tip["tag"], tip["sha"]

    tyler_version = subprocess.check_output([tylertoo, "--version"],
                                            text=True).strip()
    tip_extra = args.tippecanoe_extra.split()

    # --- datasets ---------------------------------------------------------
    datasets = dict(DATASETS)
    if args.corpus:
        try:
            datasets.update(corpus_datasets())
        except (FileNotFoundError, json.JSONDecodeError, KeyError) as exc:
            return fail(f"--corpus: cannot read the corpus manifest: {exc}")
    for spec in args.dataset:
        if "=" not in spec:
            return fail(f"--dataset expects ID=PATH, got {spec!r}")
        did, dpath = spec.split("=", 1)
        datasets[did] = {"path": Path(dpath), "layer": did,
                         "note": "supplied with --dataset"}
    if args.only:
        missing = [d for d in args.only if d not in datasets]
        if missing:
            return fail(f"unknown dataset(s): {', '.join(missing)}")
        datasets = {k: v for k, v in datasets.items() if k in args.only}

    workdir = Path(args.work) if args.work else Path(tempfile.mkdtemp(prefix="tt-e2e-"))
    workdir.mkdir(parents=True, exist_ok=True)

    # The work dir is removed on every exit path (success, a failed stage,
    # Ctrl-C) unless the caller asked to keep it or supplied it.
    try:
        results = {
            "schema_version": 1,
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "machine": machine_info(),
            "tylertoo": {"binary": tylertoo, "version": tyler_version,
                         "git_sha": git_sha()},
            "tippecanoe": {"binary": tippecanoe, "version": tip_version,
                           "expected_version": EXPECTED_TIPPECANOE_VERSION,
                           "version_mismatch_allowed": (
                               tip_version != EXPECTED_TIPPECANOE_VERSION),
                           "tag": tip_tag, "sha": tip_sha},
            "settings": {
                "min_zoom": args.min_zoom, "max_zoom": args.max_zoom,
                "tile_buffer": TILE_BUFFER, "max_tile_bytes": MAX_TILE_BYTES,
                "repeat": args.repeat, "warm_cache": True,
                "tippecanoe_extra": tip_extra,
            },
            "datasets": {},
        }

        for did, ds in datasets.items():
            src = Path(ds["path"])
            if not src.exists():
                hint = ds.get("fetch_hint",
                              "fetch: gh release download fixtures-v1 "
                              "--dir tests/fixtures/realdata/")
                print(f"skip {did}: {src} not found ({hint})", file=sys.stderr)
                continue
            zmin = ds.get("min_zoom") if ds.get("min_zoom") is not None \
                else args.min_zoom
            zmax = ds.get("max_zoom") if ds.get("max_zoom") is not None \
                else args.max_zoom
            # The manifest's recorded flags for this dataset (corpus rows),
            # appended to the parity set on tippecanoe's side and recorded.
            ds_tip_flags = ds.get("tippecanoe_flags") or []
            tip_all = tip_extra + ds_tip_flags
            print(f"\n=== {did} ===", flush=True)
            entry = {
                "input": str(src),
                "input_bytes": src.stat().st_size,
                "note": ds.get("note"),
                "layer": ds["layer"],
                "min_zoom": zmin,
                "max_zoom": zmax,
                "tippecanoe_dataset_flags": ds_tip_flags,
            }

            # --- A. tylertoo: parquet -> pmtiles -----------------------------
            tt_out = workdir / f"{did}.tylertoo.pmtiles"
            tt_report = workdir / f"{did}.tylertoo.report.json"
            print("  tylertoo tiles (parquet -> pmtiles) ...", flush=True)
            entry["tylertoo"] = repeat(
                tylertoo_argv(tylertoo, src, tt_out, ds["layer"],
                              zmin, zmax, tt_report),
                args.repeat)
            entry["tylertoo"]["output_bytes"] = tt_out.stat().st_size
            entry["tylertoo"]["per_zoom"] = per_zoom_counts(tylertoo, tt_out)
            if tt_report.exists():
                entry["tylertoo"]["phases"] = phase_split(tt_report)

            # --- A'. tylertoo with the thinning ladder off (output parity) ----
            if args.quality_matched:
                qm_out = workdir / f"{did}.tylertoo-qm.pmtiles"
                qm_report = workdir / f"{did}.tylertoo-qm.report.json"
                print("  tylertoo tiles, quality-matched "
                      f"({' '.join(QUALITY_MATCHED_FLAGS)}) ...", flush=True)
                entry["tylertoo_quality_matched"] = repeat(
                    tylertoo_argv(tylertoo, src, qm_out, ds["layer"],
                                  zmin, zmax, qm_report,
                                  QUALITY_MATCHED_FLAGS),
                    args.repeat)
                entry["tylertoo_quality_matched"]["output_bytes"] = \
                    qm_out.stat().st_size
                entry["tylertoo_quality_matched"]["per_zoom"] = \
                    per_zoom_counts(tylertoo, qm_out)
                if qm_report.exists():
                    entry["tylertoo_quality_matched"]["phases"] = \
                        phase_split(qm_report)

            # --- input conversion: parquet -> fgb ----------------------------
            converters = fgb_converters(src, workdir, did)
            if not converters:
                return fail("neither ogr2ogr nor gpio found; cannot build "
                            "tippecanoe's input. Install GDAL or gpio.")
            entry["convert_fgb_candidates"] = {}
            for tool, (argv, dst) in converters.items():
                print(f"  {tool}: parquet -> fgb ...", flush=True)
                try:
                    m = repeat(argv, args.repeat, unlink_first=dst)
                except CommandFailed as exc:
                    # A converter that cannot handle this dataset is a fact about
                    # the dataset, recorded rather than fatal — as long as one
                    # converter works, tippecanoe still gets its input.
                    print(f"    {tool} failed (recorded, not fatal): "
                          f"{first_error_line(exc.tail)}", file=sys.stderr)
                    entry["convert_fgb_candidates"][tool] = {
                        "tool": tool, "failed": True, "argv": argv,
                        "stderr_tail": exc.tail[-600:]}
                    continue
                m["output_bytes"] = dst.stat().st_size
                m["tool"] = tool
                m["path"] = str(dst)
                entry["convert_fgb_candidates"][tool] = m
            ok = [m for m in entry["convert_fgb_candidates"].values()
                  if not m.get("failed")]
            if not ok:
                return fail(f"no parquet->fgb converter succeeded for {did}; "
                            "see the converter errors printed above")
            best = min(ok, key=lambda m: m["wall_median_s"])
            entry["convert_fgb"] = best
            fgb = Path(best["path"])

            # --- C. tippecanoe: fgb -> pmtiles -------------------------------
            tp_out = workdir / f"{did}.tippecanoe-fgb.pmtiles"
            print("  tippecanoe (fgb -> pmtiles) ...", flush=True)
            entry["tippecanoe_fgb"] = repeat(
                tippecanoe_argv(tippecanoe, fgb, tp_out, ds["layer"],
                                zmin, zmax, False, tip_all),
                args.repeat)
            entry["tippecanoe_fgb"]["output_bytes"] = tp_out.stat().st_size
            entry["tippecanoe_fgb"]["per_zoom"] = per_zoom_counts(tylertoo, tp_out)

            # --- B. the honest e2e = conversion + tiling ---------------------
            entry["tippecanoe_e2e"] = {
                "wall_median_s": (entry["convert_fgb"]["wall_median_s"]
                                  + entry["tippecanoe_fgb"]["wall_median_s"]),
                "peak_rss_bytes": max_opt(entry["convert_fgb"]["peak_rss_bytes"],
                                          entry["tippecanoe_fgb"]["peak_rss_bytes"]),
                "converter": entry["convert_fgb"]["tool"],
                "note": "parquet->fgb (fastest available converter) then "
                        "fgb->pmtiles, run as two processes; peak RSS is the "
                        "larger of the two stages, not their sum",
            }

            # --- optional: ldGeoJSON + -P ------------------------------------
            if args.geojson:
                gj = workdir / f"{did}.geojsonseq"
                gconv = geojsonseq_argv(src, gj)
                if gconv is None:
                    print("  (skipping ldGeoJSON: ogr2ogr not found)", file=sys.stderr)
                else:
                    try:
                        print("  ogr2ogr: parquet -> ldGeoJSON ...", flush=True)
                        entry["convert_geojsonseq"] = repeat(gconv, args.repeat,
                                                             unlink_first=gj)
                        entry["convert_geojsonseq"]["output_bytes"] = gj.stat().st_size
                        gj_out = workdir / f"{did}.tippecanoe-geojson.pmtiles"
                        print("  tippecanoe -P (ldGeoJSON -> pmtiles) ...", flush=True)
                        entry["tippecanoe_geojsonseq"] = repeat(
                            tippecanoe_argv(tippecanoe, gj, gj_out, ds["layer"],
                                            zmin, zmax, True, tip_all),
                            args.repeat)
                        entry["tippecanoe_geojsonseq"]["output_bytes"] = \
                            gj_out.stat().st_size
                        entry["tippecanoe_geojsonseq"]["per_zoom"] = \
                            per_zoom_counts(tylertoo, gj_out)
                    except CommandFailed as exc:
                        # The ldGeoJSON leg is supporting evidence, not the
                        # headline; losing it must not lose the whole run.
                        print(f"    ldGeoJSON leg failed (recorded): {exc}",
                              file=sys.stderr)
                        entry["geojsonseq_error"] = exc.tail[-600:]
                        entry.pop("convert_geojsonseq", None)
                        entry.pop("tippecanoe_geojsonseq", None)

            # --- ratios (computed here so nobody recomputes them by hand) ----
            a = entry["tylertoo"]["wall_median_s"]
            entry["ratios"] = {
                "e2e_speedup": entry["tippecanoe_e2e"]["wall_median_s"] / a,
                "tiles_only_speedup": entry["tippecanoe_fgb"]["wall_median_s"] / a,
                "input_conversion_share_of_tippecanoe_e2e":
                    entry["convert_fgb"]["wall_median_s"]
                    / entry["tippecanoe_e2e"]["wall_median_s"],
                "archive_size_ratio_tylertoo_over_tippecanoe":
                    entry["tylertoo"]["output_bytes"]
                    / entry["tippecanoe_fgb"]["output_bytes"],
            }
            qm = entry.get("tylertoo_quality_matched")
            if qm:
                q = qm["wall_median_s"]
                entry["ratios"].update({
                    "e2e_speedup_quality_matched":
                        entry["tippecanoe_e2e"]["wall_median_s"] / q,
                    "tiles_only_speedup_quality_matched":
                        entry["tippecanoe_fgb"]["wall_median_s"] / q,
                    "archive_size_ratio_quality_matched":
                        qm["output_bytes"] / entry["tippecanoe_fgb"]["output_bytes"],
                })
            results["datasets"][did] = entry
            line = (f"  e2e {entry['ratios']['e2e_speedup']:.2f}x | "
                    f"tiles-only {entry['ratios']['tiles_only_speedup']:.2f}x")
            if qm:
                line += (f" | quality-matched tiles-only "
                         f"{entry['ratios']['tiles_only_speedup_quality_matched']:.2f}x")
            print(line, flush=True)

        # Paths in the record are provenance, not addresses: rewrite the repo
        # root and the (temporary) work directory to placeholders so a committed
        # results.json is readable by someone who is not on this machine and does
        # not leak whatever directory the run happened to live in.
        blob = json.dumps(results, indent=2, default=str)
        for prefix, token in ((str(workdir), "<work>"), (str(REPO), "<repo>")):
            blob = blob.replace(prefix, token)
        Path(args.out).write_text(blob + "\n")
        print(f"\nwrote {args.out}")

        table = markdown_table(results)
        if args.markdown:
            Path(args.markdown).write_text(table)
            print(f"wrote {args.markdown}")
        print()
        print(table)
        return 0
    finally:
        if not args.keep and not args.work:
            shutil.rmtree(workdir, ignore_errors=True)
        else:
            print(f"intermediates kept in {workdir}")


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------

def fail(msg: str) -> int:
    print(f"error: {msg}", file=sys.stderr)
    return 2


class TippecanoeUnavailable(RuntimeError):
    """No usable pinned tippecanoe; the message names the remedy."""


def resolve_tippecanoe(explicit: str | None = None) -> dict:
    """Find the pinned tippecanoe and enforce its version.

    `explicit` (from --tippecanoe / $TIPPECANOE) wins; otherwise the binary
    named in tippecanoe.lock.json (written by setup_tippecanoe.sh). Either way
    `--version` must equal EXPECTED_TIPPECANOE_VERSION unless
    TIPPECANOE_ALLOW_MISMATCH=1, so a missing lockfile never means "any binary
    will do". Shared with compare_tippecanoe.py so the benchmark and the
    parity gate cannot disagree about which tippecanoe is the baseline.

    Returns binary, version, expected_version, version_mismatch_allowed, and
    tag + sha — the last two only when the binary IS the one the lockfile
    names (a brew or --tippecanoe binary has an unknown build commit, recorded
    as None rather than borrowed from the lockfile).
    """
    lock_path = HERE / "tippecanoe.lock.json"
    lock = json.loads(lock_path.read_text()) if lock_path.exists() else {}
    binary = explicit or lock.get("binary")
    if not binary or not Path(binary).exists():
        raise TippecanoeUnavailable(
            "pinned tippecanoe not found.\n"
            "  run ./setup_tippecanoe.sh, or pass --tippecanoe PATH")

    version = subprocess.check_output([binary, "--version"],
                                      stderr=subprocess.STDOUT,
                                      text=True).strip().splitlines()[0]
    mismatch_allowed = bool(os.environ.get("TIPPECANOE_ALLOW_MISMATCH"))
    if version != EXPECTED_TIPPECANOE_VERSION and not mismatch_allowed:
        raise TippecanoeUnavailable(
            f"tippecanoe version mismatch: got {version!r}, "
            f"pinned {EXPECTED_TIPPECANOE_VERSION!r}\n"
            "  set TIPPECANOE_ALLOW_MISMATCH=1 to override (recorded "
            "in the results; say so in the write-up)")

    lock_bin = lock.get("binary")
    is_lock_binary = (bool(lock_bin)
                      and Path(lock_bin).resolve() == Path(binary).resolve())
    return {
        "binary": binary,
        "version": version,
        "expected_version": EXPECTED_TIPPECANOE_VERSION,
        "version_mismatch_allowed": version != EXPECTED_TIPPECANOE_VERSION,
        "tag": lock.get("tag") if is_lock_binary else None,
        "sha": lock.get("sha") if is_lock_binary else None,
    }


def first_error_line(text: str) -> str:
    """The first line that looks like an error, not /usr/bin/time's epilogue."""
    for line in text.splitlines():
        if re.search(r"\b(error|ERROR|failed|Failed|Traceback)\b", line):
            return line.strip()
    return (text.strip().splitlines() or ["(no output)"])[0]


def max_opt(*vals):
    vals = [v for v in vals if v]
    return max(vals) if vals else None


def git_sha() -> str | None:
    try:
        return subprocess.check_output(
            ["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"],
            text=True, stderr=subprocess.DEVNULL).strip()
    except Exception:
        return None


def phase_split(report: Path) -> dict:
    """Pull convert/export wall + counts out of `tiles --report`.

    The convert half is the parquet read + generalization ladder; the export
    half is MVT encoding and archive write. Publishing the split is what lets
    a reader see how much of tylertoo's own wall is the columnar read — the
    counterpart to tippecanoe's separate conversion step.
    """
    try:
        doc = json.loads(report.read_text())
    except Exception:
        return {}
    conv, exp = doc.get("convert", {}), doc.get("export", {})
    return {
        "convert_wall_s": conv.get("duration_secs"),
        "export_wall_s": exp.get("duration_secs"),
        "input_features": conv.get("input_features"),
        "row_groups_read": conv.get("row_groups_read"),
        "row_groups_total": conv.get("row_groups_total"),
        "total_tiles": exp.get("total_tiles"),
        "total_tile_features": exp.get("total_tile_features"),
        "oversized_tiles": exp.get("oversized_tiles"),
    }


def mb(n) -> str:
    return "—" if not n else f"{n / 1024**2:.0f}"


def markdown_table(results: dict) -> str:
    lines = []
    m = results["machine"]
    lines.append(f"Machine: {m.get('cpu')} · {m.get('cpu_count')} cores · "
                 f"{m.get('ram_gb')} GB · {m.get('platform')}")
    lines.append(f"tylertoo {results['tylertoo']['version']} "
                 f"({results['tylertoo']['git_sha']}) vs "
                 f"{results['tippecanoe']['version']} "
                 f"(pinned {results['tippecanoe']['sha']})")
    s = results["settings"]
    lines.append(f"z{s['min_zoom']}–z{s['max_zoom']}, buffer {s['tile_buffer']}, "
                 f"max tile {s['max_tile_bytes']} B, median of {s['repeat']} "
                 f"warm runs")
    lines.append("")
    lines.append("| dataset | tylertoo e2e | tylertoo e2e (quality-matched) | "
                 "tippecanoe e2e (convert+tile) | "
                 "tippecanoe tile-only (fgb in) | e2e speedup | tile-only speedup | "
                 "tile-only, quality-matched | "
                 "tylertoo RSS | tippecanoe RSS | tylertoo archive | tippecanoe archive |")
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for did, e in results["datasets"].items():
        r = e["ratios"]
        qm = e.get("tylertoo_quality_matched")
        qm_wall = f"{qm['wall_median_s']:.2f} s" if qm else "—"
        qm_ratio = (f"**{r['tiles_only_speedup_quality_matched']:.2f}×**"
                    if qm else "—")
        lines.append(
            f"| {did} "
            f"| {e['tylertoo']['wall_median_s']:.2f} s "
            f"| {qm_wall} "
            f"| {e['tippecanoe_e2e']['wall_median_s']:.2f} s "
            f"| {e['tippecanoe_fgb']['wall_median_s']:.2f} s "
            f"| **{r['e2e_speedup']:.2f}×** "
            f"| **{r['tiles_only_speedup']:.2f}×** "
            f"| {qm_ratio} "
            f"| {mb(e['tylertoo']['peak_rss_bytes'])} MB "
            f"| {mb(e['tippecanoe_fgb']['peak_rss_bytes'])} MB "
            f"| {e['tylertoo']['output_bytes'] / 1024**2:.1f} MB "
            f"| {e['tippecanoe_fgb']['output_bytes'] / 1024**2:.1f} MB |")
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    try:
        sys.exit(main())
    except CommandFailed as exc:
        sys.stderr.write("\n--- command failed ---\n")
        sys.stderr.write(" ".join(exc.argv) + "\n")
        sys.stderr.write(exc.tail + "\n")
        sys.exit(exc.rc)
