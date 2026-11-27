#!/usr/bin/env python3
"""Generate benchmark regions from committed measurements.

The marker contract is <!-- bench:<file-stem>/<renderer>[/<arg>] --> followed
by generated text and <!-- /bench -->. Inline claims stay on the marker line.
Charts carry no provenance of their own: the stamp states it once at the top,
and the runs region lists each file's in full. Unknown values are never guessed.

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
ENCODINGS = {"polygons": "GeoParquet WKB", "in-geoarrow": "native GeoArrow",
             "in-geo2": "GeoParquet GEOMETRY", "in-geo-only": "GEOMETRY without GeoParquet metadata"}
SOURCES = {"polygons": "Local file", "in-parts13": "Multi-file list",
           "in-dir13": "Partitioned directory", "in-http13": "HTTPS"}
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


def archive_size(value):
    return gb(value) if value is None or value >= 1e9 else f"{value / 1e6:,.1f} MB"


def run_label(provenance):
    if provenance.get("release_version"):
        return provenance["release_version"]
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


def table(headers, rows):
    return "\n".join(["| " + " | ".join(headers) + " |", "|" + "---|" * len(headers)]
                     + ["| " + " | ".join(map(str, row)) + " |" for row in rows])


def details(headers, rows):
    return '<details markdown><summary>The figures as a table</summary>\n\n' + table(headers, rows) + '\n\n</details>'


def bars(title, series, rows, unit, notes=None):
    """Rows are (label, values); a value may be (number, visible text).

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
    if len(series) > 1:  # one series needs no key
        out.append('<p class="tt-legend">' + ' '.join(
            f'<span class="tt-key tt-{chr(97 + i)}">{html.escape(s)}</span>' for i, s in enumerate(series)) + '</p>')
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
    out.append(details(["Dataset", *series], [[name, *map(label, values)] for name, values in rows]))
    return "\n".join(out)


def zoom_top(measurement):
    return max((int(z) for z, counts in (measurement.get("per_zoom") or {}).get("by_zoom", {}).items()
                if counts.get("tiles", 0)), default=None)


def zoom_exclusion(e):
    a, b = zoom_top(e["tylertoo"]), zoom_top(e["tippecanoe_fgb"])
    if a is None or b is None:
        return "no per-zoom tile counts were recorded"
    if a != b:
        return f"the archives reach different max zooms (tylertoo z{a}, tippecanoe z{b})"
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


def readme_claim(r):
    n, included, _ = speedup_geomean(r)
    if n is None:
        return NOT_MEASURED
    p = r["provenance"]
    return (f"{factor(n)}× faster than tippecanoe end to end, the geometric mean over "
            f"{len(included)} datasets ({run_label(p)}, {p['date'][:10]})")


def headline(r):
    n, included, _ = speedup_geomean(r)
    if n is None:
        return NOT_MEASURED
    return (f'<p class="tt-headline"><strong>{factor(n)}× faster</strong> than tippecanoe</p>\n'
            f'<p class="tt-sub">Whole builds from GeoParquet to PMTiles, '
            f'the geometric mean over {len(included)} datasets.</p>')


def speedup(r):
    n, included, excluded = speedup_geomean(r)
    if n is None:
        return NOT_MEASURED
    rows = [r["datasets"][did] for did in included]
    tile = statistics.geometric_mean(e["tippecanoe_fgb"]["wall_median_s"] / e["tylertoo"]["wall_median_s"] for e in rows)
    qm = [e["tippecanoe_e2e"]["wall_median_s"] / e["tylertoo_quality_matched"]["wall_median_s"]
          for e in rows if e.get("tylertoo_quality_matched")]
    conversion = [f"{DISPLAY.get(did, did)}: {r['datasets'][did]['convert_fgb']['wall_median_s'] / r['datasets'][did]['tippecanoe_e2e']['wall_median_s']:.1%}"
                  for did in included if r['datasets'][did].get('convert_fgb')]
    out = [f"- In the mean: {', '.join(DISPLAY.get(d, d) for d in included)}.",
           f"- Not in the mean: {'; '.join(f'{DISPLAY.get(d, d)}: {why}' for d, why in excluded.items()) or 'none'}."]
    if conversion:
        out.append("- tippecanoe's end-to-end time includes converting the GeoParquet input to FlatGeobuf "
                   f"with gpio. That conversion is this share of it: {', '.join(conversion)}.")
    out.append(f"- For tiling alone, the geometric mean is {factor(tile)}×.")
    out.append(f"- With thinning off, the geometric mean is {factor(statistics.geometric_mean(qm))}× "
               f"over {len(qm)} datasets." if qm else "- Thinning off was not measured.")
    return "\n".join(out)


def speedup_bars(r):
    rows, tab, notes = [], [], {}
    for did, e in r["datasets"].items():
        a, b = e["tylertoo"]["wall_median_s"], e["tippecanoe_e2e"]["wall_median_s"]
        reason = zoom_exclusion(e)
        caveat = (" (no per-zoom tile counts recorded)" if reason and "no per-zoom" in reason
                  else " (max zoom differs)" if reason else "")
        ratio = f"{factor(b / a)}× faster{caveat}"
        name = DISPLAY.get(did, did)
        if reason:
            notes[name] = "not in the mean"
        rows.append((name, [(a, f"{seconds(a)}, {factor(b / a)}× faster"), b]))
        qm = e.get("tylertoo_quality_matched")
        tab.append([name, f"{e['tylertoo']['phases']['input_features']:,}", seconds(a), seconds(b),
                    seconds(e["tippecanoe_fgb"]["wall_median_s"]), ratio,
                    f"{factor(b / qm['wall_median_s'])}×" if qm else "unknown",
                    archive_size(e['tylertoo'].get('output_bytes')),
                    archive_size(e['tippecanoe_fgb'].get('output_bytes'))])
    chart = bars("End-to-end wall time, shorter is faster", ["tylertoo", "tippecanoe"], rows, "s", notes)
    return chart.split('<details markdown>')[0] + details(
        ["Dataset", "Features", "tylertoo", "tippecanoe end to end", "tippecanoe tiling only",
         "Speedup", "Speedup with thinning off", "tylertoo archive", "tippecanoe archive"], tab)


def memory_bars(r):
    rows = [(DISPLAY.get(did, did), [e["tylertoo"]["peak_rss_bytes"] / 1e9,
                                  e["tippecanoe_e2e"]["peak_rss_bytes"] / 1e9])
            for did, e in r["datasets"].items()]
    return bars("Peak memory, shorter is less", ["tylertoo", "tippecanoe"], rows, "GB")


def method(r):
    p, settings = r["provenance"], r.get("settings", {})
    out = []
    if p.get("host"):
        out.append(f"- Host: {host_text(p)}.")
    for key, label in [("warmup", "Discarded warm-ups"), ("repeat", "Rounds"),
                       ("alternation", "Run order")]:
        if key in settings:
            out.append(f"- {label}: {settings[key]}.")
    if any("wall_median_s" in e.get("tylertoo", {}) for e in r["datasets"].values()):
        out.append("- Wall time: median of recorded rounds.")
    if "warm_cache" in settings:
        out.append(f"- Warm cache: {'yes' if settings['warm_cache'] else 'no'}.")
    if p.get("allocator"):
        out.append(f"- Allocator: {p['allocator']}.")
    if any(i.get("sha256") for i in p.get("inputs", [])):
        out.append(f"- Inputs: pinned by sha256 in `{r['_source_file']}`.")
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
        out.extend(['', '<details markdown><summary>Exact commands</summary>', '', *commands,
                    '', '</details>'])
    return "\n".join(out) or NOT_MEASURED


PAGE_FILES = {"e2e-corpus": "Corpus", "overture-buildings-africa": "Africa", "release-cells": "Release cells"}


def stamp(results):
    """One provenance line: each distinct run once, naming files only when runs differ."""
    groups = {}
    for stem, label in PAGE_FILES.items():
        if stem in results:
            p = results[stem]["provenance"]
            groups.setdefault((run_label(p), (p['date'] or 'undated')[:10]), []).append(label)
    if not groups:
        return NOT_MEASURED
    runs = [f"{run} on {date}" + (f" ({', '.join(labels)})" if len(groups) > 1 else "")
            for (run, date), labels in groups.items()]
    tools = results.get("e2e-corpus", {}).get("provenance", {}).get("tool_versions", {})
    tools = [f"{name} {tool_version(name, value)}" for name, value in tools.items()
             if name in ("tippecanoe", "gpio") and value is not None]
    return ('<p class="tt-stamp" markdown>Measured at ' + "; ".join(runs)
            + (f", against {' and '.join(tools)}" if tools else "")
            + ". See the [measurement method](#measurement-method).</p>")


def runs(results):
    out = [f"- {label}: {run_text(results[stem])}." for stem, label in PAGE_FILES.items() if stem in results]
    return "\n".join(out) or NOT_MEASURED


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
    return (f"streams {r['features'] / 1e6:,.0f} million Overture buildings "
            f"({gb(r['in_scope_bytes'])} of partitioned GeoParquet) from S3 into PMTiles "
            f"in {minutes(statistics.median(row['runs_wall_s']))} on one machine "
            f"({run_label(r['provenance'])})")


def hero(r):
    row, bounded = stream_rows(r)
    if not row:
        return NOT_MEASURED
    stats = [(minutes(statistics.median(row['runs_wall_s'])), "wall time"), (millions(r['features']), "features"),
             (gb(row.get('fetched_bytes')), "fetched"), (gb(row['peak_rss_bytes']), "peak memory")]
    out = ['<div class="tt-stats">\n' + '\n'.join(
        f'<p class="tt-stat"><span class="tt-stat-value">{html.escape(v)}</span> '
        f'<span class="tt-stat-label">{html.escape(k)}</span></p>' for v, k in stats) + '\n</div>']
    release = overture_release(r)
    out.append(f"\n{r['features']:,} features" + (f" from Overture release {release}." if release else "."))
    if bounded:
        same = row.get('archive_sha256') and row['archive_sha256'] == bounded.get('archive_sha256')
        out.append(f"\n`--profile bounded`: {minutes(statistics.median(bounded['runs_wall_s']))} wall time, "
                   f"{gb(bounded['peak_rss_bytes'])} peak memory" + (", same archive." if same else "."))
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
        out.append(bars(f"Africa wall time on {host(last['provenance'])}, shorter is faster", ["tylertoo"],
                        [(label(e), [wall(e) / 60]) for e in current], "min"))
    earlier = [e for group in groups.values() for e in group]
    if earlier:
        out.append('<details markdown><summary>Earlier runs on hardware that differs or was not recorded</summary>\n\n'
                   + table(["Run", "Host", "Overture release", "Wall time"],
                           [[run_label(e['provenance']), host(e['provenance']), overture_release(e) or 'unknown', minutes(wall(e))]
                            for e in earlier]) + '\n\n</details>')
    return '\n\n'.join(out)


def recipe(r):
    row, _ = stream_rows(r)
    if not row or not row.get('argv'):
        return NOT_MEASURED
    argv = ['tylertoo', *row['argv'][1:]]
    # The output is `{out}/out.pmtiles` as written in the manifest, or the
    # scratch path it expanded to in a recorded run.
    argv = ['africa.pmtiles' if arg.endswith('/out.pmtiles') else arg for arg in argv]
    env = [f"{key}={shlex.quote(str(value))}" for key, value in row.get('env', {}).items()]
    return '```bash\n' + ' '.join([*env, shlex.join(argv)]) + '\n```'


def cell_bin(r, cell):
    bins = r.get('cells', {}).get(cell, {})
    return bins.get('auto') or next(iter(bins.values()), None)


def profile_pair(r, cell):
    bins = r.get('cells', {}).get(cell, {})
    if 'auto' in bins and 'bounded' in bins:
        return bins['auto'], bins['bounded']
    return cell_bin(r, cell), cell_bin(r, cell + '-bounded')


def cell_chart(r, cells, title):
    measurements = [cell_bin(r, cell) for cell in cells]
    if not all(measurements):
        return NOT_MEASURED
    return bars(title, ['tylertoo'], [(label, [m['wall_s']['median']])
                for label, m in zip(cells.values(), measurements)], 's')


def encodings(r):
    return cell_chart(r, ENCODINGS, 'Input encodings, wall time')


def sources(r):
    out = cell_chart(r, SOURCES, 'Input sources, wall time')
    if out == NOT_MEASURED:
        return out
    return out + '\n\nFor `s3://`, see [Streaming Overture from S3](#streaming-overture-from-s3).'


def profiles(r):
    pairs = [(label, profile_pair(r, cell)) for cell, label in PROFILE_INPUTS.items()]
    if not all(a and b for _, (a, b) in pairs):
        return NOT_MEASURED
    wall = [(label, [a['wall_s']['median'], b['wall_s']['median']]) for label, (a, b) in pairs]
    peak = [(label, [a['true_peak_mib']['median'] / 1024, b['true_peak_mib']['median'] / 1024])
            for label, (a, b) in pairs]
    return (bars('Memory profiles, wall time', ['auto', 'bounded'], wall, 's') + '\n\n'
            + bars('Memory profiles, peak memory', ['auto', 'bounded'], peak, 'GiB'))


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
        'method': method, 'hero': hero, 'history': history, 'recipe': recipe,
        'encodings': encodings, 'sources': sources, 'profiles': profiles, 'compare-maps': compare_maps}.items()},
    'stamp': (lambda r, a: stamp(r), False),
    'runs': (lambda r, a: runs(r), False),
    'readme-claim': (lambda r, a: readme_claim(r), True),
    'stream-claim': (lambda r, a: stream_claim(r), True),
}
PAGE_WIDE = {'stamp', 'runs'}  # renderers that receive every results file


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
    versions = {results[s]['provenance']['tylertoo_version'].removeprefix('tylertoo ') for s in stems if s in results}
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
    if sections & {'speedup', 'speedup-bars', 'memory-bars', 'readme-claim'}:
        for did, e in r.get('datasets', {}).items():
            for tool in ['tylertoo', 'tippecanoe_e2e', 'tippecanoe_fgb', 'tylertoo_quality_matched']:
                if e.get(tool):
                    out[f'{did}/{tool}'] = e[tool]['wall_median_s']
    for section, cells in [('encodings', ENCODINGS), ('sources', SOURCES)]:
        if section in sections and all(cell_bin(r, c) for c in cells):
            for cell in cells:
                out[cell] = cell_bin(r, cell)['wall_s']['median']
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
