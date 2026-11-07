#!/usr/bin/env python3
"""gen_bench_docs.py - the one owner of every published benchmark number (#810).

Reads benchmarks/results/*.json and rewrites the marked regions in the docs:

    <!-- bench:e2e-corpus/headline -->
    ...generated, never hand-edited...
    <!-- /bench -->

The marker name is `<results-file-stem>/<section>[/<arg>]`. A region whose
renderer is inline (one sentence or number inside prose) keeps its text on the
marker line; a block region (table) is surrounded by newlines and ends with a
one-line provenance footer (commit, tylertoo version, tool versions, host,
date) taken from the file's `provenance` block.

    uv run python scripts/gen_bench_docs.py            # rewrite regions
    uv run python scripts/gen_bench_docs.py --check    # exit 1 if any differ

Updating the docs: rerun the harness, commit the JSON, run this script.

Results JSON contract (every file under benchmarks/results/):

    provenance: {commit, tylertoo_version, host, nproc, kernel, allocator,
                 tool_versions: {name: version}, inputs: [{name, sha256, ...}],
                 date}
    Unknown values are null, never guessed. `notes` (list of str) is optional.

Showcase schemas (rendered by `vida-head-to-head` and `overture-scaling`):

    vida-head-to-head: {provenance, dataset, features, input_bytes,
      rows: [{label, runs_wall_s: [..], conversion_wall_s?: float,
              peak_rss_bytes, effective_cores?: float, archive_bytes,
              note?: str}]}
      Wall shown is median(runs_wall_s) + conversion_wall_s (the end-to-end
      time of that workflow).
      The one-machine Overture showcase (benchmarks/showcase/showcase.py,
      overture-buildings-<region>.json) writes this shape. Its rows are
      tylertoo from s3://, tylertoo on the local extract, and tippecanoe
      (extract + FlatGeobuf + tile). It adds region, bbox, parts_bytes and
      extract {wall_s, peak_rss_bytes, peak_disk_bytes}; per row
      archive_sha256 [..], peak_disk_bytes, fetched_bytes (pass0, s3://
      only), steps {extract_s, fgb_convert_s, tile_s} (tippecanoe only) and
      archive {bytes, sha256, tiles, max_tile_bytes, stats: `tylertoo stats
      --json`}. provenance.inputs lists the bbox and every touched part's
      url, ETag and bytes.
    overture-scaling (multi-machine; no showcase writes it now):
      {provenance, dataset, features, dataset_bytes,
      runs: [{machines, wall_s, peak_rss_bytes_per_machine, fetched_bytes,
              local_disk_bytes, merge_s, cost_usd}],
      tippecanoe: {attempted: false, needs: [str, ...]}}
      Scaling efficiency = wall(first run) * machines(first) / (wall * machines).
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT / "benchmarks" / "results"
DOC_FILES = [
    "README.md",
    "docs/benchmarks.md",
    "docs/guides/tippecanoe.md",
    "docs/guides/scaling.md",
    "docs/OVERVIEW_TUNING.md",
    "benchmarks/e2e/RESULTS.md",
]
PROVENANCE_KEYS = ["commit", "tylertoo_version", "host", "nproc", "kernel",
                   "allocator", "tool_versions", "inputs", "date"]

DAG = " \u2020"
DASH = "\u2014"

# Display names for the benchmarks.md table; unknown ids fall back to the id.
DISPLAY = {
    "points-nyc-medium": "NYC places (point)",
    "lines-portland-medium": "Portland roads (line)",
    "lines-oregon-large": "Oregon roads (line)",
    "polygons-portland-medium": "Portland buildings (polygon)",
    "polygons-ftw-moldova-large": "Moldova parcels (polygon, dense)",
}


def s(v, unit="s", nd=2):
    return "—" if v is None else f"{v:.{nd}f} {unit}"


def mb(v):
    return "—" if not v else f"{v / 1024**2:.0f} MB"


def mbf(v, nd=1):
    return "—" if not v else f"{v / 1024**2:.{nd}f} MB"


def headline(r) -> str:
    out = ["| dataset | features | tylertoo (parquet→pmtiles) | tippecanoe e2e "
           "(convert+tile) | tippecanoe tile-only (fgb in) | e2e | tile-only |",
           "|---|---|---|---|---|---|---|"]
    for did, e in r["datasets"].items():
        feats = (e.get("tylertoo", {}).get("phases", {}) or {}).get("input_features")
        out.append(
            f"| `{did}` | {feats:,} | {s(e['tylertoo']['wall_median_s'])} "
            f"| {s(e['tippecanoe_e2e']['wall_median_s'])} "
            f"| {s(e['tippecanoe_fgb']['wall_median_s'])} "
            f"| **{e['ratios']['e2e_speedup']:.1f}×** "
            f"| **{e['ratios']['tiles_only_speedup']:.1f}×** |")
    return "\n".join(out)


def quality_matched(r) -> str:
    """The comparison to quote when output parity matters more than defaults."""
    rows = [(did, e) for did, e in r["datasets"].items()
            if e.get("tylertoo_quality_matched")]
    if not rows:
        return ("_(no quality-matched runs in this results file; re-run with "
                "`--quality-matched`)_")
    out = ["| dataset | tylertoo default | tylertoo quality-matched "
           "| tippecanoe tile-only | tile-only speedup, matched "
           "| tylertoo tiles (default → matched) | tippecanoe tiles "
           "| archive, matched | archive ratio |",
           "|---|---|---|---|---|---|---|---|---|"]
    for did, e in rows:
        qm = e["tylertoo_quality_matched"]
        d_tiles = (e["tylertoo"].get("per_zoom") or {}).get("tiles_total")
        q_tiles = (qm.get("per_zoom") or {}).get("tiles_total")
        p_tiles = (e["tippecanoe_fgb"].get("per_zoom") or {}).get("tiles_total")
        out.append(
            f"| `{did}` | {s(e['tylertoo']['wall_median_s'])} "
            f"| {s(qm['wall_median_s'])} "
            f"| {s(e['tippecanoe_fgb']['wall_median_s'])} "
            f"| **{e['ratios']['tiles_only_speedup_quality_matched']:.1f}×** "
            f"| {d_tiles:,} → {q_tiles:,} | {p_tiles:,} "
            f"| {mbf(qm['output_bytes'])} "
            f"| {e['ratios']['archive_size_ratio_quality_matched']:.2f}× |")
    return "\n".join(out)


def cpu(v):
    """CPU utilisation; >100% means multi-core. Recorded from run_e2e.py's
    /usr/bin/time wrap (older results files predate the field)."""
    return "—" if v is None else f"{v:.0f}%"


def zoom_mismatch(e) -> bool:
    """True when tippecanoe built zooms tylertoo did not (ratios not like-for-like)."""
    top = lambda m: max((int(z) for z in (m.get("per_zoom") or {}).get("by_zoom", {})), default=0)
    return top(e["tippecanoe_fgb"]) > top(e["tylertoo"])


def memory_and_size(r) -> str:
    # CPU columns appear only when the run recorded them (older files predate it).
    has_cpu = any(e["tylertoo"].get("cpu_pct") is not None
                  for e in r["datasets"].values())
    cols = ["dataset", "tylertoo peak RSS", "tippecanoe peak RSS",
            "converter peak RSS"]
    if has_cpu:
        cols += ["tylertoo CPU", "tippecanoe CPU"]
    cols += ["tylertoo archive", "tippecanoe archive", "archive ratio"]
    out = ["| " + " | ".join(cols) + " |", "|" + "---|" * len(cols)]
    for did, e in r["datasets"].items():
        row = [f"`{did}`", mb(e["tylertoo"]["peak_rss_bytes"]),
               mb(e["tippecanoe_fgb"]["peak_rss_bytes"]),
               mb(e["convert_fgb"].get("peak_rss_bytes"))]
        if has_cpu:
            row += [cpu(e["tylertoo"].get("cpu_pct")),
                    cpu(e["tippecanoe_fgb"].get("cpu_pct"))]
        row += [mbf(e["tylertoo"]["output_bytes"]),
                mbf(e["tippecanoe_fgb"]["output_bytes"]),
                f"{e['ratios']['archive_size_ratio_tylertoo_over_tippecanoe']:.2f}\u00d7"
                f"{DAG if zoom_mismatch(e) else ''}"]
        out.append("| " + " | ".join(row) + " |")
    return "\n".join(out)


def input_cost(r) -> str:
    out = ["| dataset | parquet in | fgb out | conversion (fastest tool) | tool "
           "| conversion share of tippecanoe e2e | ldGeoJSON out | ldGeoJSON convert "
           "| tippecanoe -P on ldGeoJSON |",
           "|---|---|---|---|---|---|---|---|---|"]
    for did, e in r["datasets"].items():
        gj_c = e.get("convert_geojsonseq")
        gj_t = e.get("tippecanoe_geojsonseq")
        out.append(
            f"| `{did}` | {mbf(e['input_bytes'])} "
            f"| {mbf(e['convert_fgb']['output_bytes'])} "
            f"| {s(e['convert_fgb']['wall_median_s'])} "
            f"| `{e['convert_fgb']['tool']}` "
            f"| {e['ratios']['input_conversion_share_of_tippecanoe_e2e'] * 100:.0f}% "
            f"| {mbf(gj_c['output_bytes']) if gj_c else '—'} "
            f"| {s(gj_c['wall_median_s']) if gj_c else '—'} "
            f"| {s(gj_t['wall_median_s']) if gj_t else '—'} |")
    return "\n".join(out)


def phases(r) -> str:
    out = ["| dataset | convert (parquet read + ladder) | export (MVT + archive) "
           "| row groups read/total | tiles | tile features | oversized tiles |",
           "|---|---|---|---|---|---|---|"]
    for did, e in r["datasets"].items():
        p = e["tylertoo"].get("phases") or {}
        out.append(
            f"| `{did}` | {s(p.get('convert_wall_s'))} | {s(p.get('export_wall_s'))} "
            f"| {p.get('row_groups_read')}/{p.get('row_groups_total')} "
            f"| {p.get('total_tiles'):,} | {p.get('total_tile_features'):,} "
            f"| {p.get('oversized_tiles')} |")
    return "\n".join(out)


def per_zoom(r, did) -> str:
    e = r["datasets"][did]
    tt = (e["tylertoo"].get("per_zoom") or {}).get("by_zoom", {})
    tp = (e["tippecanoe_fgb"].get("per_zoom") or {}).get("by_zoom", {})
    zooms = sorted({int(z) for z in list(tt) + list(tp)})
    out = ["| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes "
           "| tippecanoe bytes |", "|---|---|---|---|---|"]
    for z in zooms:
        a = tt.get(str(z), {})
        b = tp.get(str(z), {})
        out.append(f"| z{z} | {a.get('tiles', 0):,} | {b.get('tiles', 0):,} "
                   f"| {mbf(a.get('total_bytes'), 2)} "
                   f"| {mbf(b.get('total_bytes'), 2)} |")
    ta = sum(v.get("tiles") or 0 for v in tt.values())
    tb = sum(v.get("tiles") or 0 for v in tp.values())
    out.append(f"| **total** | **{ta:,}** | **{tb:,}** | | |")
    return "\n".join(out)


def provenance(r) -> str:
    m, t, p = r["machine"], r["tylertoo"], r["tippecanoe"]
    s_ = r["settings"]
    return "\n".join([
        f"- **Machine** — {m.get('cpu')}, {m.get('cpu_count')} cores, "
        f"{m.get('ram_gb')} GB RAM, {m.get('platform')}",
        f"- **tylertoo** — {t['version']} @ `{t['git_sha']}` (release build)",
        (f"- **tippecanoe** — {p['version']}, tag `{p['tag']}`, "
         f"commit `{p['sha']}`, built from source by `setup_tippecanoe.sh`"
         if p.get("sha") else
         f"- **tippecanoe** — {p['version']}, not the `setup_tippecanoe.sh` "
         f"build (commit unknown)"),
        f"- **Settings** — z{s_['min_zoom']}–z{s_['max_zoom']}, tile buffer "
        f"{s_['tile_buffer']}, per-tile cap {s_['max_tile_bytes']:,} bytes",
        f"- **Method** — median of {s_['repeat']} timed runs after one "
        f"discarded warm-up; warm page cache",
        f"- **Recorded** — {r['generated_at']} → "
        f"`{r.get('_source_file', 'results.json')}`",
    ])


def dataset_flags(r) -> str:
    """Per-dataset zoom range and recorded tippecanoe flags (corpus rows carry
    the flags from corpus/manifest.json; fixture rows carry none)."""
    out = ["| dataset | zooms | tippecanoe flags beyond the parity set |",
           "|---|---|---|"]
    for did, e in r["datasets"].items():
        flags = " ".join(e.get("tippecanoe_dataset_flags") or []) or "—"
        zmin = e.get("min_zoom", r["settings"]["min_zoom"])
        zmax = e.get("max_zoom", r["settings"]["max_zoom"])
        out.append(f"| `{did}` | z{zmin}–z{zmax} | `{flags}` |"
                   .replace("`—`", "—"))
    return "\n".join(out)




def variance(r, did="madagascar-adm4") -> str:
    """Run-to-run spread, derived from the wall_all_s arrays."""
    e = r["datasets"][did]
    rows = [("tylertoo, defaults", e["tylertoo"]),
            ("tylertoo, quality-matched", e.get("tylertoo_quality_matched")),
            ("tippecanoe, fgb in", e["tippecanoe_fgb"]),
            ("tippecanoe -P, ldGeoJSON in", e.get("tippecanoe_geojsonseq")),
            (f"{e['convert_fgb']['tool']} parquet→fgb", e["convert_fgb"])]
    out = ["| measurement | median | min | max | spread, (max − min) / min |",
           "|---|---|---|---|---|"]
    for name, m in rows:
        if not m:
            continue
        w = m["wall_all_s"]
        out.append(f"| {name} | {s(statistics.median(w))} | {s(min(w))} "
                   f"| {s(max(w))} | {(max(w) - min(w)) / min(w) * 100:.0f}% |")
    return "\n".join(out)


# ---- docs-facing renderers (benchmarks.md, README) ------------------------

def _is_point(did):
    return did.startswith("points-")


def tippecanoe_table(r) -> str:
    out = ["| dataset (class) | features | tylertoo | tippecanoe e2e | speedup "
           "| speedup, thinning off | peak RSS (tylertoo / tippecanoe) |",
           "|---|---|---|---|---|---|---|"]
    for did, e in r["datasets"].items():
        feats = e["tylertoo"]["phases"]["input_features"]
        out.append(
            f"| {DISPLAY.get(did, did)}{DAG if zoom_mismatch(e) else ''} | {feats:,} "
            f"| {s(e['tylertoo']['wall_median_s'])} "
            f"| {s(e['tippecanoe_e2e']['wall_median_s'])} "
            f"| **{e['ratios']['e2e_speedup']:.1f}×** "
            f"| {e['ratios']['e2e_speedup_quality_matched']:.1f}×"
            f"{'*' if zoom_mismatch(e) else ''} "
            f"| {e['tylertoo']['peak_rss_bytes'] / 1024**2:,.0f} / "
            f"{e['tippecanoe_fgb']['peak_rss_bytes'] / 1024**2:,.0f} MB |")
    return "\n".join(out)


def _line_poly(r):
    return [e for did, e in r["datasets"].items() if not _is_point(did)]


def _mem_ratios(r):
    return [e["tylertoo"]["peak_rss_bytes"] / e["tippecanoe_fgb"]["peak_rss_bytes"]
            for did, e in r["datasets"].items()
            if e["tylertoo"]["peak_rss_bytes"] and e["tippecanoe_fgb"]["peak_rss_bytes"]]


def max_memory_ratio(r) -> str:
    """Worst tylertoo/tippecanoe peak-RSS ratio over the line and polygon rows."""
    ratios = [e["tylertoo"]["peak_rss_bytes"] / e["tippecanoe_fgb"]["peak_rss_bytes"]
              for e in _line_poly(r)]
    return f"{max(ratios):.1f}×"


def readme_claim(r) -> str:
    ds = _line_poly(r)
    e2e = [e["ratios"]["e2e_speedup"] for e in ds]
    qm = [e["ratios"]["e2e_speedup_quality_matched"] for e in ds]
    feats = [e["tylertoo"]["phases"]["input_features"] for e in ds]
    p = r["provenance"]
    return (
        f"{min(e2e):.1f}–{max(e2e):.1f}× faster than tippecanoe "
        f"end-to-end on {min(feats) // 1000}k–{max(feats) // 1000}k-feature "
        f"line and polygon datasets at each tool's production settings "
        f"— {min(qm):.1f}–{max(qm):.1f}× with tylertoo's feature "
        f"thinning switched off, and at up to {max_memory_ratio(r)} peak memory "
        f"({p['host'].split(',')[0]}, tylertoo {p['tylertoo_version'].removeprefix('tylertoo ')} @ "
        f"`{p['commit']}`, tippecanoe {p['tool_versions'].get('tippecanoe')}, "
        f"{(p['date'] or 'undated')[:10]})")


# ---- showcases (schemas in the module docstring) --------------------------

def _gb(v):
    return "—" if v is None else f"{v / 1e9:,.1f} GB"


def region_name(r) -> str:
    """The dataset label with its region slug as a display name (netherlands ->
    Netherlands, new-york -> New York): the slug is an id, not prose."""
    # coordinates go in code spans, so prose lint reads no negative numbers
    name = re.sub(r"\(bbox ([-0-9.,]+)\)", r"(bbox `\1`)", r["dataset"])
    slug = r.get("region")
    if not slug:
        return name
    return name.replace(f", {slug} (", f", {slug.replace('-', ' ').title()} (")


def vida_head_to_head(r) -> str:
    out = [f"{region_name(r)}: {r['features']:,} features, {_gb(r['input_bytes'])}.",
           "",
           "| tool | end-to-end wall | peak RSS | effective cores | archive | note |",
           "|---|---|---|---|---|---|"]
    for row in r["rows"]:
        wall = statistics.median(row["runs_wall_s"]) + (row.get("conversion_wall_s") or 0)
        ec = row.get("effective_cores")
        out.append(f"| {row['label']} | {wall:,.0f} s | {_gb(row['peak_rss_bytes'])} "
                   f"| {'—' if ec is None else f'{ec:.1f}'} "
                   f"| {_gb(row['archive_bytes'])} | {row.get('note') or ''} |")
    return "\n".join(out)


def _min(sec):
    return f"{sec / 60:,.0f} min"


def stream_at_scale(r) -> str:
    """A streaming-only record: tylertoo from s3:// to PMTiles, one row per profile."""
    out = [f"{region_name(r)}: {r['features']:,} features kept by `--bbox`. The bbox touches "
           f"{r['parts_touched']} of the release's partitioned GeoParquet files"
           + (f" and {r['row_groups_read']:,} of its {r['row_groups_total']:,} row groups"
              if r.get("row_groups_read") else "")
           + f"; those row groups hold {_gb(r['in_scope_bytes'])} of the release's "
             f"{_gb(r['release_bytes'])}.",
           "",
           "| run | wall | peak RSS | effective cores | fetched | peak local disk | archive |",
           "|---|---|---|---|---|---|---|"]
    for row in r["rows"]:
        wall = statistics.median(row["runs_wall_s"])
        ec = row.get("effective_cores")
        out.append(f"| {row['label']} | {_min(wall)} ({wall:,.0f} s) | {_gb(row['peak_rss_bytes'])} "
                   f"| {'—' if ec is None else f'{ec:.1f}'} | {_gb(row.get('fetched_bytes'))} "
                   f"| {_gb(row.get('peak_disk_bytes'))} | {_gb(row['archive_bytes'])} |")
    return "\n".join(out)


def fast_disk(r) -> str:
    """A showcase record's fast-disk re-runs (tylertoo only)."""
    out = ["| tylertoo run | wall | peak RSS | effective cores |", "|---|---|---|---|"]
    for row in r.get("fast_disk_rows", []):
        wall = statistics.median(row["runs_wall_s"])
        ec = row.get("effective_cores")
        out.append(f"| {row['label'].removeprefix('tylertoo, ')} | {wall:,.0f} s "
                   f"| {_gb(row['peak_rss_bytes'])} | {'—' if ec is None else f'{ec:.1f}'} |")
    return "\n".join(out)


def stream_claim(r) -> str:
    """One inline sentence for the README."""
    wall = statistics.median(r["rows"][0]["runs_wall_s"])
    return (f"streams {r['features'] / 1e6:,.0f} million Overture buildings "
            f"({_gb(r['in_scope_bytes'])} of partitioned GeoParquet) from S3 into PMTiles "
            f"in {_min(wall)} on one machine")


PROFILE_INPUTS = {"tiles-madagascar": "Madagascar admin level 4, z0–12",
                  "polygons": "Moldova parcels, z0–14",
                  "tiles-nz": "NZ parcels, z0–14"}


def _profile_rows(r):
    for cell, name in PROFILE_INPUTS.items():
        a, b = r["cells"][cell]["auto"], r["cells"][cell]["bounded"]
        yield (name, a["wall_s"]["median"], b["wall_s"]["median"],
               a["true_peak_mib"]["median"], b["true_peak_mib"]["median"], a["outputs"] == b["outputs"])


def memory_profiles(r) -> str:
    """`--profile auto` (the default) against `bounded`, alternating rounds."""
    out = ["| input | `auto` wall | `bounded` wall | bounded costs | `auto` peak | `bounded` peak | same archive |",
           "|---|---|---|---|---|---|---|"]
    for name, aw, bw, ap, bp, same in _profile_rows(r):
        out.append(f"| {name} | {aw:,.2f} s | {bw:,.2f} s | {(bw / aw - 1) * 100:+.0f}% "
                   f"| {ap / 1024:,.1f} GiB | {bp / 1024:,.1f} GiB | {'yes' if same else 'no'} |")
    return "\n".join(out)


def bounded_cost(r) -> str:
    """The range of bounded's slowdown, inline."""
    c = sorted((bw / aw - 1) * 100 for _, aw, bw, *_ in _profile_rows(r))
    return f"{c[0]:.0f}–{c[-1]:.0f}%"


def overture_scaling(r) -> str:
    out = [f"{r['dataset']}: {r['features']:,} features, {_gb(r['dataset_bytes'])}.",
           "",
           "| machines | wall | peak RSS per machine | fetched / dataset "
           "| local disk | merge | efficiency | cost |",
           "|---|---|---|---|---|---|---|---|"]
    base = r["runs"][0]
    for x in r["runs"]:
        eff = base["wall_s"] * base["machines"] / (x["wall_s"] * x["machines"])
        out.append(
            f"| {x['machines']} | {x['wall_s'] / 60:,.1f} min "
            f"| {_gb(x['peak_rss_bytes_per_machine'])} "
            f"| {x['fetched_bytes'] / r['dataset_bytes']:.2f}× "
            f"| {_gb(x['local_disk_bytes'])} | {x['merge_s']:,.0f} s "
            f"| {eff * 100:.0f}% | ${x['cost_usd']:,.2f} |")
    t = r["tippecanoe"]
    out += ["", "tippecanoe: not attempted. The same output would need "
            + "; ".join(t["needs"]) + "."]
    return "\n".join(out)


def brazil_table(r) -> str:
    def hms(sec):
        h, rem = divmod(int(sec), 3600)
        m, sc = divmod(rem, 60)
        return (f"{h} h " if h else "") + f"{m} m {sc} s"
    out = ["| Stage | Wall time | Peak resident memory | Output |",
           "| --- | --- | --- | --- |"]
    for st in r["stages"]:
        out.append(f"| {st['name']} | {hms(st['wall_s'])} "
                   f"| {st['peak_rss_gib']} GiB | {st['output']} |")
    return "\n".join(out)


# name -> (renderer taking (results, arg), inline?, footer?)
RENDERERS = {
    "headline": (lambda r, a: headline(r), False, True),
    "quality-matched": (lambda r, a: quality_matched(r), False, True),
    "memory": (lambda r, a: memory_and_size(r), False, True),
    "input": (lambda r, a: input_cost(r), False, True),
    "phases": (lambda r, a: phases(r), False, True),
    "flags": (lambda r, a: dataset_flags(r), False, True),
    "provenance": (lambda r, a: provenance(r), False, False),
    "per-zoom": (lambda r, a: per_zoom(r, a), False, True),
    "variance": (lambda r, a: variance(r, a or "madagascar-adm4"), False, True),
    "tippecanoe-table": (lambda r, a: tippecanoe_table(r), False, True),
    "max-memory-ratio": (lambda r, a: max_memory_ratio(r), True, False),
    "readme-claim": (lambda r, a: readme_claim(r), True, False),
    "vida-head-to-head": (lambda r, a: vida_head_to_head(r), False, True),
    "overture-scaling": (lambda r, a: overture_scaling(r), False, True),
    "stream-at-scale": (lambda r, a: stream_at_scale(r), False, True),
    "stream-claim": (lambda r, a: stream_claim(r), True, False),
    "fast-disk": (lambda r, a: fast_disk(r), False, True),
    "memory-profiles": (lambda r, a: memory_profiles(r), False, True),
    "bounded-cost": (lambda r, a: bounded_cost(r), True, False),
    "brazil-table": (lambda r, a: brazil_table(r), False, True),
}
SECTIONS = ["headline", "quality-matched", "memory", "input", "phases", "flags",
            "provenance"]


def footer(r) -> str:
    p = r["provenance"]
    tools = ", ".join(v if str(v).startswith(k) else f"{k} {v}"
                      for k, v in p["tool_versions"].items()
                      # run_manifest names each --bin label; tylertoo builds are in the run line already
                      if k != "tippecanoe_commit" and v != p["tylertoo_version"])
    return (f"_Run: commit {p['commit'] or 'unknown'}, tylertoo "
            f"{p['tylertoo_version'].removeprefix('tylertoo ')}, {tools + ', ' if tools else ''}{p['host']}, "
            f"{(p['date'] or 'undated')[:10]}._")


def check_provenance(name, r):
    missing = [k for k in PROVENANCE_KEYS if k not in r.get("provenance", {})]
    if missing:
        raise SystemExit(f"{name}: provenance is missing {missing}")


def load_results(results_dir=RESULTS_DIR) -> dict:
    out = {}
    for f in sorted(Path(results_dir).glob("*.json")):
        r = json.loads(f.read_text())
        check_provenance(f.name, r)
        r["_source_file"] = f.name
        out[f.stem] = r
    return out


MARKER = re.compile(r"<!-- bench:(\S+) -->(.*?)<!-- /bench -->", re.S)


def render_region(name, results) -> str:
    stem, _, rest = name.partition("/")
    section, _, arg = rest.partition("/")
    if stem not in results or section not in RENDERERS:
        raise SystemExit(f"unknown bench region: {name}")
    fn, inline, foot = RENDERERS[section]
    r = results[stem]
    body = fn(r, arg or None)
    if inline:
        return body
    return "\n" + body + ("\n\n" + footer(r) if foot else "") + "\n"


def rewrite(text, results) -> str:
    return MARKER.sub(
        lambda m: f"<!-- bench:{m[1]} -->{render_region(m[1], results)}<!-- /bench -->",
        text)


def run(root=ROOT, results_dir=RESULTS_DIR, check=False, files=DOC_FILES) -> list:
    """Rewrite (or, with check, just compare) every doc; return stale paths."""
    results = load_results(results_dir)
    stale = []
    for rel in files:
        path = Path(root) / rel
        if not path.exists():
            continue
        old = path.read_text()
        new = rewrite(old, results)
        if new != old:
            stale.append(rel)
            if not check:
                path.write_text(new)
    return stale


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[1])
    ap.add_argument("--check", action="store_true",
                    help="exit 1 if any region differs from what would be generated")
    args = ap.parse_args()
    stale = run(check=args.check)
    for rel in stale:
        print(("stale: " if args.check else "rewrote: ") + rel)
    return 1 if (args.check and stale) else 0


if __name__ == "__main__":
    sys.exit(main())
