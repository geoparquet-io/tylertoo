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
thins features at coarse and mid zooms; tippecanoe drops only points, so on a
contiguous polygon coverage it emits every feature at every zoom. Measuring A
against C on such a dataset compares against less work. A' turns the ladder
off (keeping simplification) and lands within a couple of tiles of
tippecanoe's per-zoom counts.

The conversion step (B − C) is printed on its own so a reader can see exactly
how much of the e2e gap is input format. A line-delimited GeoJSON variant is
also measured (`--geojson`) because "fgb is tippecanoe's best input" is a
claim that should carry evidence, and because ldGeoJSON + `-P` is what most
real GeoParquet→tippecanoe pipelines actually run.

FAIRNESS RULES THIS SCRIPT FOLLOWS
----------------------------------
* tippecanoe is pinned (see setup_tippecanoe.sh) and its version + git SHA go
  into every result file. Never "whatever is on PATH".
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
}

# --------------------------------------------------------------------------
# Settings parity
#
# These are tylertoo's `tiles` defaults, mapped onto the nearest tippecanoe
# flag. Where a default already matches (the 500 KB per-tile cap) nothing is
# passed on either side. Where tylertoo's default differs from tippecanoe's
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
MAX_TILE_BYTES = 500_000   # tylertoo --max-tile-size 500K == tippecanoe default


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
    return {
        "argv": argv,
        "runs": n,
        "wall_median_s": statistics.median(walls),
        "wall_min_s": min(walls),
        "wall_max_s": max(walls),
        "peak_rss_bytes": max(rss) if rss else None,
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
# thins features at coarse and mid zooms; tippecanoe drops nothing but points,
# so for a contiguous polygon coverage (admin boundaries, parcels) tippecanoe
# emits every feature at every zoom and tylertoo emits a sample. On the
# madagascar fixture that is 865 of 17,465 polygons at z8 — a visibly
# different map, and comparing wall time against it would be comparing
# against less work.
#
# `--verbatim` switches the whole thinning/visibility ladder off, and
# `--simplify-factor` is then set back explicitly (verbatim would otherwise
# disable simplification too, and tippecanoe does simplify). `--max-tile-size`
# likewise has to be restated because verbatim disables the cap. The result
# emits every feature at every zoom, which is what tippecanoe does here, and
# lands within a handful of tiles of tippecanoe's per-zoom counts.
QUALITY_MATCHED_FLAGS = ["--verbatim", "--simplify-factor", "1.0"]


def tylertoo_argv(binary, src, dst, layer, zmin, zmax, report, extra=()):
    return [
        binary, "tiles", str(src), str(dst),
        "--min-zoom", str(zmin),
        "--max-zoom", str(zmax),
        "--layer-name", layer,
        "--tile-buffer", str(TILE_BUFFER),
        "--max-tile-size", str(MAX_TILE_BYTES),
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
    ap.add_argument("--min-zoom", type=int, default=DEFAULT_MIN_ZOOM)
    ap.add_argument("--max-zoom", type=int, default=DEFAULT_MAX_ZOOM)
    ap.add_argument("--tylertoo", default=os.environ.get(
        "TYLERTOO", str(REPO / "target" / "release" / "tylertoo")))
    ap.add_argument("--tippecanoe", default=os.environ.get("TIPPECANOE"),
                    help="default: the binary named in tippecanoe.lock.json")
    ap.add_argument("--tippecanoe-extra", default="",
                    help="extra tippecanoe flags, space-separated, appended to "
                         "the parity set and recorded in results.json "
                         "(e.g. --tippecanoe-extra=-pf). Pass it with '=' or "
                         "argparse will swallow the leading dash. For probing "
                         "how much a single flag costs — publish the parity "
                         "set, not a variant, unless you say which")
    ap.add_argument("--quality-matched", action="store_true",
                    help="also run tylertoo with the thinning ladder off "
                         f"({' '.join(QUALITY_MATCHED_FLAGS)}) so it emits "
                         "every feature at every zoom, as tippecanoe does "
                         "for polygons/lines. This is the comparison to quote "
                         "when output parity matters more than defaults")
    ap.add_argument("--geojson", action="store_true",
                    help="also measure the ldGeoJSON + -P tippecanoe path")
    ap.add_argument("--work", default=None,
                    help="working directory for intermediates (default: a temp dir)")
    ap.add_argument("--keep", action="store_true",
                    help="keep intermediates and output archives")
    ap.add_argument("--out", default=str(HERE / "results.json"))
    ap.add_argument("--markdown", default=None,
                    help="also write a markdown table here")
    args = ap.parse_args()

    # --- resolve binaries -------------------------------------------------
    tylertoo = args.tylertoo
    if not Path(tylertoo).exists():
        return fail(f"tylertoo binary not found: {tylertoo}\n"
                    f"  build it: cargo build --release -p tylertoo")

    lock_path = HERE / "tippecanoe.lock.json"
    lock = json.loads(lock_path.read_text()) if lock_path.exists() else {}
    tippecanoe = args.tippecanoe or lock.get("binary")
    if not tippecanoe or not Path(tippecanoe).exists():
        return fail("pinned tippecanoe not found.\n"
                    "  run ./setup_tippecanoe.sh, or pass --tippecanoe PATH")

    tip_version = subprocess.check_output([tippecanoe, "--version"],
                                          stderr=subprocess.STDOUT,
                                          text=True).strip().splitlines()[0]
    pinned = lock.get("version_string")
    if pinned and tip_version != pinned and not os.environ.get("TIPPECANOE_ALLOW_MISMATCH"):
        return fail(f"tippecanoe version mismatch: got {tip_version!r}, "
                    f"pinned {pinned!r}\n"
                    "  set TIPPECANOE_ALLOW_MISMATCH=1 to override (and say so "
                    "in the results)")

    tyler_version = subprocess.check_output([tylertoo, "--version"],
                                            text=True).strip()
    tip_extra = args.tippecanoe_extra.split()

    # --- datasets ---------------------------------------------------------
    datasets = dict(DATASETS)
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

    results = {
        "schema_version": 1,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "machine": machine_info(),
        "tylertoo": {"binary": tylertoo, "version": tyler_version,
                     "git_sha": git_sha()},
        "tippecanoe": {"binary": tippecanoe, "version": tip_version,
                       "tag": lock.get("tag"), "sha": lock.get("sha")},
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
            print(f"skip {did}: {src} not found "
                  f"(fetch: gh release download fixtures-v1 "
                  f"--dir tests/fixtures/realdata/)", file=sys.stderr)
            continue
        print(f"\n=== {did} ===", flush=True)
        entry = {
            "input": str(src),
            "input_bytes": src.stat().st_size,
            "note": ds.get("note"),
            "layer": ds["layer"],
        }

        # --- A. tylertoo: parquet -> pmtiles -----------------------------
        tt_out = workdir / f"{did}.tylertoo.pmtiles"
        tt_report = workdir / f"{did}.tylertoo.report.json"
        print("  tylertoo tiles (parquet -> pmtiles) ...", flush=True)
        entry["tylertoo"] = repeat(
            tylertoo_argv(tylertoo, src, tt_out, ds["layer"],
                          args.min_zoom, args.max_zoom, tt_report),
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
                              args.min_zoom, args.max_zoom, qm_report,
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
                        f"see {args.out} for the recorded errors")
        best = min(ok, key=lambda m: m["wall_median_s"])
        entry["convert_fgb"] = best
        fgb = Path(best["path"])

        # --- C. tippecanoe: fgb -> pmtiles -------------------------------
        tp_out = workdir / f"{did}.tippecanoe-fgb.pmtiles"
        print("  tippecanoe (fgb -> pmtiles) ...", flush=True)
        entry["tippecanoe_fgb"] = repeat(
            tippecanoe_argv(tippecanoe, fgb, tp_out, ds["layer"],
                            args.min_zoom, args.max_zoom, False, tip_extra),
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
                                        args.min_zoom, args.max_zoom, True, tip_extra),
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

    if not args.keep and not args.work:
        shutil.rmtree(workdir, ignore_errors=True)
    else:
        print(f"intermediates kept in {workdir}")
    return 0


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------

def fail(msg: str) -> int:
    print(f"error: {msg}", file=sys.stderr)
    return 2


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
