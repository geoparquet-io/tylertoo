#!/usr/bin/env python3
"""Generate benchmark regions from committed measurements.

The marker contract is <!-- bench:<file-stem>/<renderer>[/<arg>] --> followed
by generated text and <!-- /bench -->. Inline claims stay on the marker line.
Charts carry no provenance of their own: the runs region at the foot of the
page states it once, with each file's in full. Unknown values are never guessed.

Every JSON has provenance: commit, tylertoo_version, host, nproc, kernel,
allocator, tool_versions, inputs (name and sha256), and date. Optional fields
are release_version (only release candidates or exact tags), git_describe,
host_class, and notes. Labels prefer release_version, then git_describe,
then commit; a binary version alone does not identify a release run.

Remaining schemas:
- e2e-corpus: datasets keyed by id, with tylertoo, tippecanoe_e2e,
  tippecanoe_fgb, convert_fgb, and optional tylertoo_quality_matched. Each
  measurement records wall_median_s, optional wall_all_s, peak_rss_bytes,
  output_bytes, argv, and per_zoom.by_zoom tile counts. settings records the
  method; provenance.inputs pins inputs.
- overture-buildings-africa: features, in_scope_bytes, dataset, region, bbox,
  release_bytes, rows with label, runs_wall_s, peak_rss_bytes, fetched_bytes,
  archive_sha256, archive, argv (binary, command, args, zoom flags, flags),
  and env. argv retains {out} placeholders. Optional history is a list of
  {provenance, release, wall_s} snapshots (or full stream documents with rows).
  release identifies the Overture release; older files encode it in dataset.
- release-cells: run_manifest.py report --json-out, cells keyed by cell then
  binary label; wall_s.median and true_peak_mib.median are whole-run metrics.
  Profiles are either auto/bounded bins or separate X/X-bounded cells.
- comparison-pmtiles: provenance, version, base_url, datasets list with id,
  label, layer, geometry, and tylertoo/tippecanoe objects containing url,
  bytes, sha256, min_zoom, max_zoom, and argv. URLs use <id>/<tool>.pmtiles.

Run with --check to validate freshness and coverage, or --diff REF for ranked
whole-run wall changes against committed results at REF.
"""

from __future__ import annotations

import argparse
import html
import json
import re
import shlex
import statistics
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RESULTS_DIR = ROOT / "benchmarks/results"
DOC_FILES = ["README.md", "crates/core/README.md", "crates/cli/README.md",
             "crates/python/README.md", "docs/benchmarks.md",
             "docs/guides/tippecanoe.md"]
PROSE_LINE_CAP = 40
PROVENANCE_KEYS = ["commit", "tylertoo_version", "host", "nproc", "kernel",
                   "allocator", "tool_versions", "inputs", "date"]
NOT_MEASURED = "_Not measured yet. `benchmarks/release.sh` fills this chart._"
DISPLAY = {
    "points-nyc-medium": "New York City places (point)",
    "lines-oregon-large": "Oregon roads (line)",
    "polygons-portland-medium": "Portland buildings (polygon)",
    "polygons-ftw-moldova-large": "Moldova parcels (polygon, dense)",
}
PROFILE_INPUTS = {"tiles-madagascar": "Madagascar admin boundaries", "polygons": "Moldova parcels",
                  "tiles-nz": "New Zealand parcels"}
PLACES = {"points-nyc-medium": "New York City", "lines-oregon-large": "Oregon",
          "polygons-portland-medium": "Portland", "polygons-ftw-moldova-large": "Moldova"}
MARKER = re.compile(r"<!-- bench:(\S+) -->(.*?)<!-- /bench -->", re.S)


def factor(value):
    return f"{value:.0f}" if value >= 10 else f"{value:.1f}"


def seconds(value):
    return f"{value:,.1f} s"


def minutes(value):
    return f"{value / 60:,.0f} min"


def millions(value):
    return f"{value / 1e6:,.0f} million" if value >= 1e6 else f"{value:,}"


def gb(value):
    return "unknown" if value is None else f"{value / 1e9:,.1f} GB"


def run_label(provenance):
    if provenance.get("release_version"):
        version = str(provenance["release_version"])
        return version if version.startswith("v") else f"v{version}"
    describe = provenance.get("git_describe") or ""
    # A describe past a release tag: say plainly that the code is unreleased.
    m = re.fullmatch(r"(v[^-]+)-\d+-g([0-9a-f]+)(-dirty)?", describe)
    if m:
        return (f"an unreleased commit {m[2]} after {m[1]}"
                + (", with local changes" if m[3] else ""))
    return describe or f"commit {provenance.get('commit') or 'unknown'}"


def tool_version(name, value):
    if name == "gpio" and isinstance(value, str):
        match = re.fullmatch(r"geoparquet-io, version (\d+\.\d+\.\d+[\w.+-]*)", value.strip())
        if match:
            return match[1]
    return value


def host_text(p):
    """The host string plus its memory, which newer host strings leave out."""
    host, mem = p.get("host") or "unknown host", p.get("mem_gib")
    return host if mem is None or "RAM" in host else f"{host}, {mem} GiB RAM"


ARCH = {"aarch64": "ARM64", "arm64": "ARM64", "x86_64": "x86-64"}


def host_prose(p):
    """'an ARM64 host with 64 CPUs and 247 GiB of RAM', or the raw host string."""
    m = re.fullmatch(r"([^,]+), CPU [^,]+, (\d+) CPUs(?:, (\d+) GiB RAM)?", p.get("host") or "")
    if not m:
        return f"on {host_text(p)}"
    arch = ARCH.get(m[1], m[1])
    mem = m[3] or p.get("mem_gib")
    article = "an" if arch[0].upper() in "AEFHILMNORSX" else "a"
    return (f"on {article} {arch} host with {m[2]} CPUs"
            + (f" and {mem} GiB of RAM" if mem is not None else ""))


def long_date(p):
    date = (p.get("date") or "")[:10]
    m = re.fullmatch(r"(\d{4})-(\d{2})-(\d{2})", date)
    if not m:
        return "an unrecorded date"
    months = ["January", "February", "March", "April", "May", "June", "July", "August",
              "September", "October", "November", "December"]
    return f"{months[int(m[2]) - 1]} {int(m[3])}, {m[1]}"


def words(n):
    return ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine"][n] if 0 <= n < 10 else str(n)


def series(items):
    items = list(items)
    if len(items) < 3:
        return " and ".join(items)
    return ", ".join(items[:-1]) + ", and " + items[-1]


def run_text(r):
    p = r["provenance"]
    label = run_label(p)
    commit = f"commit {p.get('commit') or 'unknown'}"
    tools = [f"{k} {tool_version(k, v)}" for k, v in p["tool_versions"].items()
             if k in ("tippecanoe", "gpio") and v is not None]
    # The unreleased-commit label already names the commit.
    fields = [label] + ([] if (p.get('commit') or 'unknown') in label else [commit]) + tools
    fields += [f"allocator: {p['allocator']}", host_text(p), (p["date"] or "undated")[:10]]
    return ", ".join(str(v) for v in fields)


def table(headers, rows, align=None):
    """A Markdown table; align is one 'l' or 'r' per column."""
    rule = "|" + "".join("---:|" if a == "r" else "---|" for a in (align or "l" * len(headers)))
    return "\n".join(["| " + " | ".join(headers) + " |", rule]
                      + ["| " + " | ".join(map(str, row)) + " |" for row in rows])


def bars(title, series_names, rows, unit, notes=None):
    """Rows are (label, values); a value may be (number, visible text).

    Every value is printed at its bar's tip, so the chart needs no table.
    notes maps a label to a short text marker shown beside it; such rows are
    also drawn hatched, so the marker never relies on color alone.
    """
    notes = notes or {}

    def number(value):
        return value[0] if isinstance(value, tuple) else value

    def label(value):
        return value[1] if isinstance(value, tuple) else f"{value:,.1f} {unit}"

    maximum = max((number(v) for _, values in rows for v in values), default=0)
    out = [f'<figure class="tt-chart"><figcaption>{html.escape(title)}</figcaption>']
    if len(series_names) > 1:  # one series needs no key
        out.append('<p class="tt-legend">' + ' '.join(
            f'<span class="tt-key tt-{chr(97 + i)}">{html.escape(s)}</span>' for i, s in enumerate(series_names)) + '</p>')
    for name, values in rows:
        note = notes.get(name)
        out.append(f'<div class="tt-row{" tt-excluded" if note else ""}"><span class="tt-label">{html.escape(name)}'
                   + (f' <span class="tt-note">{html.escape(note)}</span>' if note else '') + '</span>')
        for i, value in enumerate(values):
            share = number(value) / maximum if maximum else 0
            out.append(f'<span class="tt-line"><span class="tt-bar tt-{chr(97 + i)}" style="--f:{share:.3f}" '
                       f'aria-hidden="true"></span><span class="tt-val">{html.escape(label(value))}</span></span>')
        out.append('</div>')
    out.append('</figure>')
    return "\n".join(out)


def short_name(did):
    """The chart label: the dataset without its geometry note."""
    return DISPLAY.get(did, did).split(" (")[0]


def zoom_top(measurement):
    return max((int(z) for z, counts in (measurement.get("per_zoom") or {}).get("by_zoom", {}).items()
                if counts.get("tiles", 0)), default=None)


def zoom_exclusion(e):
    a, b = zoom_top(e["tylertoo"]), zoom_top(e["tippecanoe_fgb"])
    if a is None or b is None:
        return "no per-zoom tile counts were recorded"
    if a != b:
        return f"the archives end at different zoom levels, z{a} for tylertoo and z{b} for tippecanoe"
    return None


def speedup_geomean(r):
    included, excluded, ratios = [], {}, []
    for did, e in r["datasets"].items():
        reason = zoom_exclusion(e)
        if reason:
            excluded[did] = reason
        else:
            included.append(did)
            ratios.append(e["tippecanoe_e2e"]["wall_median_s"] / e["tylertoo"]["wall_median_s"])
    return (statistics.geometric_mean(ratios) if ratios else None), included, excluded


def ordered(r):
    """Datasets in the mean first, then the ones shown separately."""
    _, included, excluded = speedup_geomean(r)
    return included + list(excluded)


def readme_claim(r):
    n, included, _ = speedup_geomean(r)
    if n is None:
        return NOT_MEASURED
    # README claims carry no version, date, or method; the linked page states them.
    return f"{factor(n)}× faster than tippecanoe"


def headline(r):
    n, included, _ = speedup_geomean(r)
    if n is None:
        return NOT_MEASURED
    peaks = [(e["tylertoo"]["peak_rss_bytes"], e["tippecanoe_e2e"]["peak_rss_bytes"])
             for e in r["datasets"].values()]
    if all(b < a for a, b in peaks):
        memory = "tippecanoe used less peak memory on each dataset."
    elif all(a < b for a, b in peaks):
        memory = "tylertoo used less peak memory on each dataset."
    elif all(a == b for a, b in peaks):
        memory = "Both tools used the same peak memory on each dataset."
    else:
        memory = (f"tylertoo used less peak memory on {words(sum(a < b for a, b in peaks))} "
                  f"of {words(len(peaks))} datasets.")
    return (f"tylertoo was **{factor(n)}× faster than tippecanoe** across {words(len(included))} "
            f"GeoParquet-to-PMTiles builds. {memory}")


def speedup(r):
    n, included, excluded = speedup_geomean(r)
    if n is None:
        return NOT_MEASURED
    places = [PLACES.get(did, short_name(did)) for did in included]
    out = [f"The {factor(n)}× figure is the geometric mean across {series(places)}."]
    for did, why in excluded.items():
        out.append(f"The {PLACES.get(did, short_name(did))} runs are shown separately because {why}.")
    return " ".join(out)


def speedup_bars(r):
    rows, notes = [], {}
    _, _, excluded = speedup_geomean(r)
    for did in ordered(r):
        e = r["datasets"][did]
        a, b = e["tylertoo"]["wall_median_s"], e["tippecanoe_e2e"]["wall_median_s"]
        if did in excluded:
            notes[short_name(did)] = "not in the mean"
        rows.append((short_name(did), [(a, f"{seconds(a)}, {factor(b / a)}× faster"), b]))
    return bars("Build time", ["tylertoo", "tippecanoe"], rows, "s", notes)


def memory_bars(r):
    rows = [(short_name(did), [r["datasets"][did]["tylertoo"]["peak_rss_bytes"] / 1e9,
                         r["datasets"][did]["tippecanoe_e2e"]["peak_rss_bytes"] / 1e9])
            for did in ordered(r)]
    return bars("Peak memory", ["tylertoo", "tippecanoe"], rows, "GB")


PAGE_FILES = {"e2e-corpus": "tylertoo vs. tippecanoe", "overture-buildings-africa": "Overture buildings from S3",
              "release-cells": "Memory profiles"}
RESULTS_URL = "https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/results/"


def archive_size(value):
    return gb(value) if value is None or value >= 1e9 else f"{value / 1e6:,.1f} MB"


def figures(r):
    """Every recorded corpus figure, one row per dataset."""
    n, _, excluded = speedup_geomean(r)
    if n is None:
        return NOT_MEASURED
    rows = []
    for did in ordered(r):
        e = r["datasets"][did]
        a, b = e["tylertoo"]["wall_median_s"], e["tippecanoe_e2e"]["wall_median_s"]
        qm = e.get("tylertoo_quality_matched")
        conv = e.get("convert_fgb")
        rows.append([DISPLAY.get(did, did) + (", not in the mean" if did in excluded else ""),
                     f"{e['tylertoo']['phases']['input_features']:,}" if e["tylertoo"].get("phases") else "unknown",
                     seconds(a), seconds(b), seconds(e["tippecanoe_fgb"]["wall_median_s"]),
                     seconds(conv["wall_median_s"]) if conv else "unknown",
                     f"{factor(b / a)}×", f"{factor(b / qm['wall_median_s'])}×" if qm else "unknown",
                     archive_size(e["tylertoo"].get("output_bytes")),
                     archive_size(e["tippecanoe_fgb"].get("output_bytes"))])
    headers = ["Dataset", "Features", "tylertoo", "tippecanoe end to end", "tippecanoe tiling only",
               "gpio conversion", "Speedup", "Speedup, thinning off", "tylertoo archive", "tippecanoe archive"]
    return table(headers, rows, align="l" + "r" * (len(headers) - 1))


def same_run(a, b):
    keys = ("commit", "git_describe", "release_version", "host", "mem_gib", "allocator")
    return all(a.get(k) == b.get(k) for k in keys) and (a.get("date") or "")[:10] == (b.get("date") or "")[:10]


def runs(results):
    """One row per results file on the page; a build and host the runs share is stated once."""
    present = [(stem, label) for stem, label in PAGE_FILES.items() if stem in results]
    if not present:
        return NOT_MEASURED
    provs = [results[stem]["provenance"] for stem, _ in present]

    def tools(stem):
        p = results[stem]["provenance"]
        return ", ".join(f"{k} {tool_version(k, v)}" for k, v in p["tool_versions"].items()
                         if k in ("tippecanoe", "gpio") and v is not None) or "none"

    def link(stem):
        f = results[stem]["_source_file"]
        return f"[`{f}`]({RESULTS_URL}{f})"

    def build(p):
        label, commit = run_label(p), p.get('commit') or 'unknown'
        return label if commit in label else f"{label}, commit `{commit}`"

    if all(same_run(q, provs[0]) for q in provs):
        p = provs[0]
        lead = (f"Every run used tylertoo {build(p)}, built with `{p.get('allocator') or 'unknown'}`, "
                f"on {host_text(p)}, on {(p.get('date') or 'an unrecorded date')[:10]}.")
        return lead + "\n\n" + table(["Section", "Compared with", "Recorded results"],
                                      [[label, tools(stem), link(stem)] for stem, label in present])
    return table(["Section", "tylertoo", "Compared with", "Host", "Allocator", "Date", "Recorded results"],
                 [[label, build(results[stem]["provenance"]), tools(stem), host_text(results[stem]["provenance"]),
                   results[stem]["provenance"].get("allocator") or "unknown",
                   (results[stem]["provenance"].get("date") or "undated")[:10], link(stem)]
                  for stem, label in present])


def method(r):
    p, settings = r["provenance"], r.get("settings", {})
    timing = "Each time is the median of " + (f"{words(settings['repeat'])} runs" if "repeat" in settings
                                              else "the recorded runs")
    if "warmup" in settings:
        timing += f" after {words(settings['warmup'])} discarded warm-up runs"
    if "warm_cache" in settings:
        timing += " with a warm cache" if settings["warm_cache"] else " with a cold cache"
    out = [f"- {timing}."]
    if "alternation" in settings:
        out.append(f"- Run order: {settings['alternation']}.")
    if any(i.get("sha256") for i in p.get("inputs", [])):
        out.append(f"- Input checksums are pinned in `{r['_source_file']}`.")
    _, included, excluded = speedup_geomean(r)
    rows = [r["datasets"][did] for did in included]
    conversion = [f"{PLACES.get(did, short_name(did))} {r['datasets'][did]['convert_fgb']['wall_median_s'] / r['datasets'][did]['tippecanoe_e2e']['wall_median_s']:.1%}"
                  for did in included if r["datasets"][did].get("convert_fgb")]
    if conversion:
        out.append("- tippecanoe's time includes converting GeoParquet to FlatGeobuf with `gpio`. "
                   f"That step is this share of it: {series(conversion)}.")
    if rows:
        tile = statistics.geometric_mean(e["tippecanoe_fgb"]["wall_median_s"] / e["tylertoo"]["wall_median_s"]
                                         for e in rows)
        out.append(f"- For tiling alone, without the conversion, the geometric mean is {factor(tile)}×.")
        qm = [e["tippecanoe_e2e"]["wall_median_s"] / e["tylertoo_quality_matched"]["wall_median_s"]
              for e in rows if e.get("tylertoo_quality_matched")]
        if qm:
            out.append(f"- With thinning off (`--verbatim --simplify-factor 1.0`), the geometric mean is "
                       f"{factor(statistics.geometric_mean(qm))}×.")
    for did, why in excluded.items():
        out.append(f"- {PLACES.get(did, short_name(did))} is not in the mean: {why}.")
    commands = []
    for did, e in r["datasets"].items():
        group = []
        for tool in ["tylertoo", "tippecanoe_fgb", "convert_fgb", "tylertoo_quality_matched"]:
            argv = e.get(tool, {}).get("argv")
            if argv:
                label = {'tylertoo': 'tylertoo', 'tippecanoe_fgb': 'tippecanoe tiling only',
                         'convert_fgb': 'GeoParquet to FlatGeobuf',
                         'tylertoo_quality_matched': 'tylertoo with thinning off'}[tool]
                group.append(f"    - {label}: `{shlex.join(argv)}`.")
        if group:
            commands.extend([f"- {DISPLAY.get(did, did)}:", *group])
    if commands:
        out.extend(['', '<details markdown><summary>Exact commands for tylertoo vs. tippecanoe</summary>', '',
                    *commands, '', '</details>'])
    return "\n".join(out)


def stream_rows(r):
    rows = r.get("rows", [])
    default = next((row for row in rows if 'bounded' not in row.get('label', '')
                    and 'bounded' not in row.get('argv', [])), None)
    bounded = next((row for row in rows if 'bounded' in row.get('label', '')
                    or 'bounded' in row.get('argv', [])), None)
    return default, bounded


def stream_claim(r):
    row, _ = stream_rows(r)
    if not row:
        return NOT_MEASURED
    wall = statistics.median(row['runs_wall_s']) / 60
    return (f"Capable of streaming {r['features'] / 1e6:,.0f} million Overture buildings "
            f"({gb(r['in_scope_bytes'])}) from S3 into PMTiles in {wall:,.0f} minutes")


def flag_value(argv, flag):
    for i, arg in enumerate(argv):
        if arg == flag and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith(flag + "="):
            return arg.split("=", 1)[1]
    return None


def hero(r):
    row, bounded = stream_rows(r)
    if not row:
        return NOT_MEASURED
    tiles = [(minutes(statistics.median(row['runs_wall_s'])), "build time"), (millions(r['features']), "buildings"),
             (gb(row.get('fetched_bytes')), "fetched from S3"), (gb(row['peak_rss_bytes']), "peak memory")]
    out = ['<div class="tt-stats">\n' + '\n'.join(
        f'<p class="tt-stat"><span class="tt-stat-value">{html.escape(v)}</span> '
        f'<span class="tt-stat-label">{html.escape(k)}</span></p>' for v, k in tiles) + '\n</div>']
    if bounded:
        same = row.get('archive_sha256') and row['archive_sha256'] == bounded.get('archive_sha256')
        out.append(f"\nWith `--profile bounded`, the build took {minutes(statistics.median(bounded['runs_wall_s']))} "
                   f"and peaked at {gb(bounded['peak_rss_bytes'])}" + (", with the same archive." if same else "."))
    return '\n'.join(out)


def overture_release(r):
    match = re.search(r'\d{4}-\d{2}-\d{2}\.\d+', r.get('dataset', ''))
    return r.get('release') or (match[0] if match else None)


def history_host(provenance):
    """Recover legacy host classes only from the harness's exact host shape."""
    host_class = provenance.get('host_class')
    if not host_class:
        match = re.fullmatch(r'([^,]+), CPU (.+), (\d+) CPUs', provenance.get('host') or '')
        if match:
            host_class = '/'.join(match.groups())
    mem = provenance.get('mem_gib')
    return (host_class, mem) if host_class and mem is not None else None


def history(r):
    row, _ = stream_rows(r)
    if not row:
        return NOT_MEASURED
    entries = [*r.get('history', []), {"provenance": r['provenance'], "release": overture_release(r),
                                     "wall_s": statistics.median(row['runs_wall_s'])}]
    groups = {}
    for i, entry in enumerate(entries):
        # Unknown memory or host never matches, including another unknown.
        key = history_host(entry['provenance']) or ('unknown', i)
        groups.setdefault(key, []).append(entry)
    current_key = history_host(r['provenance']) or ('unknown', len(entries) - 1)
    current = groups.pop(current_key)

    def wall(entry):
        return (entry['wall_s'] if entry.get('wall_s') is not None
                else statistics.median(stream_rows(entry)[0]['runs_wall_s']))

    def label(entry):
        text = run_label(entry['provenance'])
        if overture_release(entry) != overture_release(r):
            text += f" (different Overture release: {overture_release(entry) or 'unknown'})"
        return text

    def host(prov):
        text = host_text({**prov, 'host': prov.get('host') or prov.get('host_class')})
        return text if 'RAM' in text else text + ', memory not recorded'

    out = []
    # The hero states the current run; chart it only against comparable runs.
    if len(current) > 1:
        first, last = current[0], current[-1]
        out.append(f"{minutes(wall(first))} at {run_label(first['provenance'])} → {minutes(wall(last))} "
                   f"at {run_label(last['provenance'])}, {factor(wall(first) / wall(last))}× faster.")
        out.append(bars(f"Overture build time on {host(last['provenance'])}", ["tylertoo"],
                        [(label(e), [wall(e) / 60]) for e in current], "min"))
    earlier = [e for group in groups.values() for e in group]
    if earlier:
        out.append('<details markdown><summary>Earlier Overture runs on other or unrecorded hardware</summary>\n\n'
                   + table(["Run", "Host", "Overture release", "Wall time"],
                           [[run_label(e['provenance']), host(e['provenance']), overture_release(e) or 'unknown', minutes(wall(e))]
                            for e in earlier]) + '\n\n</details>')
    return '\n\n'.join(out)


def command_lines(env, argv):
    """A recorded command as continued lines: environment, the binary and its
    subcommand, then one positional or flag per line."""
    first = next((i for i, arg in enumerate(argv) if arg.startswith('--')), len(argv))
    lines = ([' '.join(env)] if env else []) + [shlex.join(argv[:2])]
    lines += ['  ' + shlex.quote(arg) for arg in argv[2:first]]
    i = first
    while i < len(argv):
        part = [argv[i]]
        if '=' not in argv[i] and i + 1 < len(argv) and not argv[i + 1].startswith('--'):
            part.append(argv[i + 1])
            i += 1
        lines.append('  ' + shlex.join(part))
        i += 1
    return ' \\\n'.join(lines)


def recipe(r):
    row, _ = stream_rows(r)
    if not row or not row.get('argv'):
        return NOT_MEASURED
    argv = ['tylertoo', *row['argv'][1:]]
    # The output is `{out}/out.pmtiles` as written in the manifest, or the
    # scratch path it expanded to in a recorded run.
    argv = ['africa.pmtiles' if arg.endswith('/out.pmtiles') else arg for arg in argv]
    env = [f"{key}={shlex.quote(str(value))}" for key, value in row.get('env', {}).items()]
    release = overture_release(r)
    lead = ("The run used " + (f"Overture release `{release}` and processed " if release else "")
            + f"{r['features']:,} features.")
    return lead + '\n\n```bash\n' + command_lines(env, argv) + '\n```'


def cell_bin(r, cell):
    bins = r.get('cells', {}).get(cell, {})
    return bins.get('auto') or next(iter(bins.values()), None)


def profile_pair(r, cell):
    bins = r.get('cells', {}).get(cell, {})
    if 'auto' in bins and 'bounded' in bins:
        return bins['auto'], bins['bounded']
    return cell_bin(r, cell), cell_bin(r, cell + '-bounded')


def gib(value):
    return f"{value:,.1f} GiB"


def profiles(r):
    pairs = [(label, profile_pair(r, cell)) for cell, label in PROFILE_INPUTS.items()]
    if not all(a and b for _, (a, b) in pairs):
        return NOT_MEASURED
    wall = [(label, [a['wall_s']['median'], b['wall_s']['median']]) for label, (a, b) in pairs]
    peak = [(label, [a['true_peak_mib']['median'] / 1024, b['true_peak_mib']['median'] / 1024])
            for label, (a, b) in pairs]
    lower = sum(b < a for _, (a, b) in peak)
    slower = sum(b > a for _, (a, b) in wall)
    if lower == len(pairs) and slower == len(pairs):
        lead = "The `bounded` profile uses less peak memory and takes longer to build."
        label, (pa, pb) = max(peak, key=lambda item: item[1][0])
        wa, wb = dict(wall)[label]
        lead += (f" On {label}, it reduced peak memory from {gib(pa)} to {gib(pb)} "
                 f"and increased build time from {seconds(wa)} to {seconds(wb)}.")
    else:
        lead = (f"The `bounded` profile used less peak memory on {words(lower)} of {words(len(pairs))} "
                f"datasets and took longer on {words(slower)}.")
    return (lead + '\n\n' + bars('Build time', ['auto', 'bounded'], wall, 's') + '\n\n'
            + bars('Peak memory', ['auto', 'bounded'], peak, 'GiB'))


def compare_maps(r):
    base = f"https://data.source.coop/nlebovits/tylertoo-benchmarks/{r['version']}/"
    if r['base_url'] != base:
        raise ValueError('comparison base URL does not match version')
    out = ['<label for="compare-dataset">Dataset</label>', '<select id="compare-dataset">']
    rows = []
    for d in r['datasets']:
        for tool in ['tylertoo', 'tippecanoe']:
            if d[tool]['url'] != f"{base}{d['id']}/{tool}.pmtiles":
                raise ValueError(f"comparison URL does not match dataset/tool: {d['id']}/{tool}")
        attrs = {'tylertoo': d['tylertoo']['url'], 'tippecanoe': d['tippecanoe']['url'],
                 'layer': d.get('layer', d['id']), 'geometry': d['geometry']}
        label = DISPLAY.get(d['id'], d.get('label', d['id']))
        out.append('<option value="' + html.escape(d['id'], quote=True) + '" ' + ' '.join(
            f'data-{key}="{html.escape(value, quote=True)}"' for key, value in attrs.items()) + '>'
            + html.escape(label) + '</option>')
        rows.append([label, *[f"[{tool}]({d[tool]['url']})" for tool in ['tylertoo', 'tippecanoe']]])
    return '\n'.join([*out, '</select>', '', table(['Dataset', 'tylertoo archive', 'Tippecanoe archive'], rows),
                      '', f'_Run: {run_text(r)}._'])


RENDERERS = {
    **{name: (lambda r, a, fn=fn: fn(r), False) for name, fn in {
        'headline': headline, 'speedup': speedup, 'speedup-bars': speedup_bars, 'memory-bars': memory_bars,
        'method': method, 'hero': hero, 'history': history, 'recipe': recipe, 'figures': figures,
        'profiles': profiles, 'compare-maps': compare_maps}.items()},
    'runs': (lambda r, a: runs(r), False),
    'readme-claim': (lambda r, a: readme_claim(r), True),
    'stream-claim': (lambda r, a: stream_claim(r), True),
}
PAGE_WIDE = {'runs'}  # renderers that receive every results file


def check_provenance(name, r):
    missing = [k for k in PROVENANCE_KEYS if k not in r.get('provenance', {})]
    if missing:
        raise SystemExit(f'{name}: provenance is missing {missing}')


def load_results(results_dir=RESULTS_DIR):
    out = {}
    for f in sorted(Path(results_dir).glob('*.json')):
        r = json.loads(f.read_text())
        check_provenance(f.name, r)
        r['_source_file'] = f.name
        out[f.stem] = r
    return out


def render_region(name, results):
    stem, _, rest = name.partition('/')
    section, _, arg = rest.partition('/')
    if section not in RENDERERS:
        raise SystemExit(f'unknown bench region: {name}')
    fn, inline = RENDERERS[section]
    r = results.get(stem)
    if r is None:
        body = NOT_MEASURED
    else:
        if section == 'compare-maps':
            corpus = results.get('e2e-corpus', {})
            if r['provenance']['tylertoo_version'] != corpus.get('provenance', {}).get('tylertoo_version'):
                raise ValueError('comparison tylertoo_version differs from e2e-corpus')
            if not {d['id'] for d in r['datasets']} <= set(corpus.get('datasets', {})):
                raise ValueError('comparison ids are not a subset of e2e-corpus datasets')
        body = fn(results if section in PAGE_WIDE else r, arg or None)
    return body if inline else '\n' + body + '\n'


def rewrite(text, results):
    return MARKER.sub(lambda m: f'<!-- bench:{m[1]} -->{render_region(m[1], results)}<!-- /bench -->', text)


def prose_errors(text):
    text = MARKER.sub('', text)
    text = re.sub(r'(?ms)^\s*(`{3,}|~{3,}).*?^\s*\1[^\n]*$', '', text)
    text = re.sub(r'(`+).*?\1', '', text, flags=re.S)
    text = re.sub(r'<!--.*?-->', '', text, flags=re.S)
    text = re.sub(r'\]\([^)]*\)', ']', text)
    text = re.sub(r'\{#[^}]*\}', '', text)  # heading anchors
    text = re.sub(r'\bS3\b', '', text)
    errors = []
    if re.search(r'[0-9]', text):
        errors.append('hand-written benchmark prose contains an ASCII digit')
    if sum(bool(line.strip()) for line in text.splitlines()) > PROSE_LINE_CAP:
        errors.append(f'hand-written benchmark prose exceeds {PROSE_LINE_CAP} non-blank lines')
    return errors


def marker_files(root):
    root = Path(root)
    try:
        res = subprocess.run(['git', 'ls-files', '*.md'], cwd=root, capture_output=True, text=True, check=True)
        paths = [root / rel for rel in res.stdout.splitlines()]
    except (OSError, subprocess.CalledProcessError):
        paths = root.rglob('*.md')
    return {p.relative_to(root).as_posix() for p in paths if p.is_file()
            and not {'target', '.git', 'node_modules', 'context'} & set(p.relative_to(root).parts)
            and '<!-- bench:' in p.read_text()}


def page_warnings(root, rel, stems, results):
    # A release run names its version; otherwise the binary's version stands in.
    versions = {str(results[s]['provenance'].get('release_version') or '').removeprefix('v')
                or results[s]['provenance']['tylertoo_version'].removeprefix('tylertoo ')
                for s in stems if s in results}
    cargo = Path(root) / 'Cargo.toml'
    if cargo.exists():
        current = tomllib.loads(cargo.read_text())['workspace']['package']['version']
        for version in sorted(versions - {current}):
            print(f'::warning::{rel}: measured at {version}; this branch is {current}; run benchmarks/release.sh')
    if len(versions) > 1:
        print(f"::warning::{rel}: page mixes versions: {', '.join(sorted(versions))}")


def page_regions(root, files):
    regions = {}
    for rel in files:
        path = Path(root) / rel
        if path.exists():
            for m in MARKER.finditer(path.read_text()):
                stem, section, *_ = m[1].split('/')
                regions.setdefault(stem, set()).add(section)
    return regions


def tutorial_tile_count(root=ROOT):
    """The tile count in the Brazil tutorial's step-one expected output."""
    path = Path(root) / 'examples/brazil-fields/README.md'
    block = re.search(r'^File: `01-tiles\.sh`\s*\n```bash\n(?:(?!^```).)*^```[ \t]*\n[ \t]*```text\n(.*?)^```',
                      path.read_text(), re.M | re.S)
    count = re.search(r'^\s*([\d,]+) tiles across z\d+\.\.z\d+\b', block[1], re.M) if block else None
    if not count:
        raise SystemExit(f'{path}: step-one expected tile count missing')
    return int(count[1].replace(',', ''))


def check_tutorial_map(root, results):
    record = results.get('tutorial-map')
    if record is None:
        print('::warning::tutorial-map.json absent; re-run benchmarks/release.sh tutorial-map docs to verify the tutorial.')
        return
    expected = tutorial_tile_count(root)
    actual = record.get('archive', {}).get('tiles')
    if actual != expected:
        raise SystemExit(f'tutorial-map.json has {actual} tiles; examples/brazil-fields/README.md expects {expected:,} tiles; '
                         're-run benchmarks/release.sh tutorial-map docs or update the tutorial.')


def run(root=ROOT, results_dir=RESULTS_DIR, check=False, files=DOC_FILES):
    results = load_results(results_dir)
    stale, used = [], set()
    if check:
        uncovered = marker_files(root) - set(files)
        if uncovered:
            raise SystemExit(f'bench markers outside DOC_FILES: {sorted(uncovered)}')
    for rel in files:
        path = Path(root) / rel
        if not path.exists():
            if check:
                raise SystemExit(f'{rel}: missing DOC_FILES entry')
            continue
        old = path.read_text()
        matches = list(MARKER.finditer(old))
        if check and not matches:
            raise SystemExit(f'{rel}: DOC_FILES entry has no bench marker')
        stems = {m[1].split('/')[0] for m in matches}
        used |= stems
        if check:
            if rel == 'docs/benchmarks.md':
                errors = prose_errors(old)
                if errors:
                    raise SystemExit(f'{rel}: ' + '; '.join(errors))
            # Stamp receives every file used on the benchmark page.
            page_stems = stems | (PAGE_FILES.keys() & results.keys()
                                  if any(m[1].split('/')[1] in PAGE_WIDE for m in matches) else set())
            page_warnings(root, rel, page_stems, results)
        new = rewrite(old, results)
        if new != old:
            stale.append(rel)
            if not check:
                path.write_text(new)
    if check:
        # The comparison maps' page may not be listed yet; validate its manifest anyway.
        if 'comparison-pmtiles' in results and 'comparison-pmtiles' not in used:
            render_region('comparison-pmtiles/compare-maps', results)
        orphans = set(results) - used - {'comparison-pmtiles', 'tutorial-map'}
        if orphans:
            raise SystemExit(f'orphan results JSON without a rendered region: {sorted(orphans)}')
        check_tutorial_map(root, results)
    return stale


def charted_walls(r, sections):
    out = {}
    if sections & {'headline', 'speedup', 'speedup-bars', 'memory-bars', 'figures', 'readme-claim'}:
        for did, e in r.get('datasets', {}).items():
            for tool in ['tylertoo', 'tippecanoe_e2e', 'tippecanoe_fgb', 'tylertoo_quality_matched']:
                if e.get(tool):
                    out[f'{did}/{tool}'] = e[tool]['wall_median_s']
    if 'profiles' in sections:
        for cell in PROFILE_INPUTS:
            a, b = profile_pair(r, cell)
            if a and b:
                out[f'{cell}/auto'], out[f'{cell}/bounded'] = a['wall_s']['median'], b['wall_s']['median']
    if sections & {'hero', 'history', 'stream-claim'}:
        for row in r.get('rows', []):
            out[row['label']] = statistics.median(row['runs_wall_s'])
    return out


def diff_results(ref, results, regions, root=ROOT):
    changes = []
    for stem, sections in regions.items():
        if stem not in results or not charted_walls(results[stem], sections):
            continue
        try:
            res = subprocess.run(['git', 'show', f'{ref}:benchmarks/results/{stem}.json'],
                                 cwd=root, capture_output=True, text=True, check=True)
        except (OSError, subprocess.CalledProcessError):
            print(f'Note: {stem}: no results at {ref}; skipped.')
            continue
        old, new = json.loads(res.stdout), results[stem]
        a, b = old['provenance'].get('host_class'), new['provenance'].get('host_class')
        if not a or not b or a != b:
            print(f"Note: {stem}: {'unknown' if not a or not b else 'different'} host_class; skipped.")
            continue
        a, b = old['provenance'].get('mem_gib'), new['provenance'].get('mem_gib')
        if a is None or b is None or a != b:
            print(f"Note: {stem}: {'unknown' if a is None or b is None else 'different'} mem_gib; skipped.")
            continue
        before = charted_walls(old, sections)
        for row, after in charted_walls(new, sections).items():
            if before.get(row):
                changes.append((after / before[row] - 1, f'{stem}/{row}', before[row], after))
    print(table(['Row', 'Wall before → after', 'Change'], [
        [name, f'{seconds(before)} → {seconds(after)}', f'{change:+.1%}']
        for change, name, before, after in sorted(changes, reverse=True)]))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--check', action='store_true', help='validate generated regions and page coverage')
    ap.add_argument('--diff', metavar='REF', help='print ranked whole-run wall changes against a git ref')
    args = ap.parse_args()
    if args.diff:
        diff_results(args.diff, load_results(), page_regions(ROOT, DOC_FILES))
        return 0
    stale = run(check=args.check)
    for rel in stale:
        print(('stale: ' if args.check else 'rewrote: ') + rel)
    return int(args.check and bool(stale))


if __name__ == '__main__':
    sys.exit(main())
