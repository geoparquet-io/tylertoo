#!/usr/bin/env python3
"""
format_results.py — turn results.json into the tables RESULTS.md carries.

Kept separate from run_e2e.py so the numbers in RESULTS.md are regenerated
from the recorded run rather than retyped. Retyped benchmark numbers drift,
and a drifted number is indistinguishable from a dishonest one.

    python3 format_results.py results.json            # print all tables
    python3 format_results.py results.json --section headline
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


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


def memory_and_size(r) -> str:
    out = ["| dataset | tylertoo peak RSS | tippecanoe peak RSS | converter peak RSS "
           "| tylertoo archive | tippecanoe archive | archive ratio |",
           "|---|---|---|---|---|---|---|"]
    for did, e in r["datasets"].items():
        out.append(
            f"| `{did}` | {mb(e['tylertoo']['peak_rss_bytes'])} "
            f"| {mb(e['tippecanoe_fgb']['peak_rss_bytes'])} "
            f"| {mb(e['convert_fgb'].get('peak_rss_bytes'))} "
            f"| {mbf(e['tylertoo']['output_bytes'])} "
            f"| {mbf(e['tippecanoe_fgb']['output_bytes'])} "
            f"| {e['ratios']['archive_size_ratio_tylertoo_over_tippecanoe']:.2f}× |")
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
        f"- **tippecanoe** — {p['version']}, tag `{p['tag']}`, "
        f"commit `{p['sha']}`, built from source by `setup_tippecanoe.sh`",
        f"- **Settings** — z{s_['min_zoom']}–z{s_['max_zoom']}, tile buffer "
        f"{s_['tile_buffer']}, per-tile cap {s_['max_tile_bytes']:,} bytes",
        f"- **Method** — median of {s_['repeat']} timed runs after one "
        f"discarded warm-up; warm page cache; no other load on the machine",
        f"- **Recorded** — {r['generated_at']} → `results.json`",
    ])


SECTIONS = {
    "headline": headline,
    "quality-matched": quality_matched,
    "memory": memory_and_size,
    "input": input_cost,
    "phases": phases,
    "provenance": provenance,
}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("results", type=Path, nargs="?",
                    default=Path(__file__).parent / "results.json")
    ap.add_argument("--section", choices=[*SECTIONS, "per-zoom"], default=None)
    ap.add_argument("--dataset", default=None, help="for --section per-zoom")
    args = ap.parse_args()
    r = json.loads(args.results.read_text())

    if args.section == "per-zoom":
        did = args.dataset or next(iter(r["datasets"]))
        print(per_zoom(r, did))
        return 0
    if args.section:
        print(SECTIONS[args.section](r))
        return 0
    for name, fn in SECTIONS.items():
        print(f"\n### {name}\n")
        print(fn(r))
    for did in r["datasets"]:
        print(f"\n### per-zoom: {did}\n")
        print(per_zoom(r, did))
    return 0


if __name__ == "__main__":
    sys.exit(main())
