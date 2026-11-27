#!/usr/bin/env python3
"""Generate benchmark regions from committed measurements.

The marker contract is <!-- bench:<file-stem>/<renderer>[/<arg>] --> followed
by generated text and <!-- /bench -->. Inline claims stay on the marker line.
Block regions end with provenance. Unknown values are never guessed.

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
             "crates/python/README.md", "docs/benchmarks.md"]
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
             "in-geo2": "GeoParquet GEOMETRY", "in-geo-only": "GEOMETRY without geo metadata"}
SOURCES = {"polygons": "Local file", "in-parts13": "Multi-file list",
           "in-dir13": "Partitioned directory", "in-http13": "HTTPS"}
MARKER = re.compile(r"<!-- bench:(\S+) -->(.*?)<!-- /bench -->", re.S)


def factor(value):
    return f"{value:.0f}" if value >= 10 else f"{value:.1f}"


def seconds(value):
    return f"{value:,.1f} s"


def minutes(value):
    return f"{value / 60:,.0f} min"


def gb(value):
    return "unknown" if value is None else f"{value / 1e9:,.1f} GB"


def archive_size(value):
    return gb(value) if value is None or value >= 1e9 else f"{value / 1e6:,.1f} MB"


def run_label(provenance):
    return (provenance.get("release_version") or provenance.get("git_describe")
            or f"commit {provenance.get('commit') or 'unknown'}")


def tool_version(name, value):
    if name == "gpio" and isinstance(value, str):
        match = re.fullmatch(r"geoparquet-io, version (\d+\.\d+\.\d+[\w.+-]*)", value.strip())
        if match:
            return match[1]
    return value


def footer(r):
    p = r["provenance"]
    label = run_label(p)
    commit = f"commit {p.get('commit') or 'unknown'}"
    tools = [f"{k} {tool_version(k, v)}" for k, v in p["tool_versions"].items()
             if k in ("tippecanoe", "gpio") and v is not None]
    fields = [label] + ([] if label == commit else [commit]) + tools
    fields += [f"allocator: {p['allocator']}", p["host"], (p["date"] or "undated")[:10]]
    return "_Run: " + ", ".join(str(v) for v in fields) + "._"


def table(headers, rows):
    return "\n".join(["| " + " | ".join(headers) + " |", "|" + "---|" * len(headers)]
                     + ["| " + " | ".join(map(str, row)) + " |" for row in rows])


def details(headers, rows):
    return '<details markdown><summary>The figures as a table</summary>\n\n' + table(headers, rows) + '\n\n</details>'


def bars(title, series, rows, unit):
    """Rows are (label, values); a value may be (number, visible text)."""
    def number(value):
        return value[0] if isinstance(value, tuple) else value

    def label(value):
        return value[1] if isinstance(value, tuple) else f"{value:,.1f} {unit}"

    maximum = max((number(v) for _, values in rows for v in values), default=0)
    out = [f'<figure class="tt-chart"><figcaption>{html.escape(title)}</figcaption>',
           '<p class="tt-legend">' + ' '.join(
               f'<span class="tt-key tt-{chr(97 + i)}">{html.escape(s)}</span>'
               for i, s in enumerate(series)) + '</p>']
    for name, values in rows:
        out.append(f'<div class="tt-row"><span class="tt-label">{html.escape(name)}</span>')
        for i, value in enumerate(values):
            width = number(value) / maximum * 100 if maximum else 0
            out.append(f'<span class="tt-bar tt-{chr(97 + i)}" style="--w:{width:.1f}%" aria-hidden="true"></span>'
                       f'<span class="tt-val">{html.escape(label(value))}</span>')
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
    return (f"tylertoo is **{factor(n)}×** faster than tippecanoe end to end, "
            f"the geometric mean over {len(included)} datasets.\n\n"
            f"Included: {', '.join(DISPLAY.get(d, d) for d in included)}. "
            f"Excluded: {'; '.join(f'{DISPLAY.get(d, d)}: {why}' for d, why in excluded.items()) or 'none'}.\n\n"
            + (f"GeoParquet to FlatGeobuf conversion as a share of tippecanoe's total wall time: {', '.join(conversion)}. " if conversion else "")
            + f"For tiling alone, the geometric mean is {factor(tile)}×. "
            + (f"With thinning off, the geometric mean is {factor(statistics.geometric_mean(qm))}× "
               f"over {len(qm)} datasets." if qm else "Thinning off was not measured."))


def speedup_bars(r):
    rows, tab = [], []
    for did, e in r["datasets"].items():
        a, b = e["tylertoo"]["wall_median_s"], e["tippecanoe_e2e"]["wall_median_s"]
        reason = zoom_exclusion(e)
        caveat = (" (no per-zoom tile counts recorded)" if reason and "no per-zoom" in reason
                  else " (max zoom differs)" if reason else "")
        ratio = f"{factor(b / a)}× faster{caveat}"
        name = DISPLAY.get(did, did)
        rows.append((name, [(a, f"{seconds(a)}, {ratio}"), b]))
        qm = e.get("tylertoo_quality_matched")
        tab.append([name, f"{e['tylertoo']['phases']['input_features']:,}", seconds(a), seconds(b),
                    seconds(e["tippecanoe_fgb"]["wall_median_s"]), ratio,
                    f"{factor(b / qm['wall_median_s'])}×" if qm else "unknown",
                    archive_size(e['tylertoo'].get('output_bytes')),
                    archive_size(e['tippecanoe_fgb'].get('output_bytes'))])
    chart = bars("End-to-end wall time, shorter is faster", ["tylertoo", "tippecanoe"], rows, "s")
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
        out.append(f"- Host: {p['host']}.")
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


def stamp(results):
    names = {"e2e-corpus": "Corpus", "overture-buildings-africa": "Africa", "release-cells": "Release cells"}
    out = []
    for stem, label in names.items():
        if stem in results:
            p = results[stem]["provenance"]
            out.append(f"{label}: {run_label(p)}, {(p['date'] or 'undated')[:10]}.")
    tools = results.get("e2e-corpus", {}).get("provenance", {}).get("tool_versions", {})
    out += [f"{name} {tool_version(name, value)}." for name, value in tools.items() if name in ("tippecanoe", "gpio")]
    return " ".join(out) or NOT_MEASURED


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
    values = [f"{r['features']:,} features", f"{minutes(statistics.median(row['runs_wall_s']))} wall time",
              f"{gb(row['peak_rss_bytes'])} peak memory", f"{gb(row.get('fetched_bytes'))} fetched"]
    out = ['<div class="tt-hero" markdown>\n\n' + '. '.join(values) + '.\n\n</div>']
    if bounded:
        same = row.get('archive_sha256') and row['archive_sha256'] == bounded.get('archive_sha256')
        out.append(f"\n`--profile bounded`: {minutes(statistics.median(bounded['runs_wall_s']))} wall time, "
                   f"{gb(bounded['peak_rss_bytes'])} peak memory" + (", same archive." if same else "."))
    release = overture_release(r)
    if release:
        out.append(f"\nOverture release: {release}.")
    return '\n'.join(out)


def overture_release(r):
    match = re.search(r'\d{4}-\d{2}-\d{2}\.\d+', r.get('dataset', ''))
    return r.get('release') or (match[0] if match else None)


def history(r):
    row, _ = stream_rows(r)
    if not row:
        return NOT_MEASURED
    entries = [*r.get('history', []), {"provenance": r['provenance'], "release": overture_release(r),
                                     "wall_s": statistics.median(row['runs_wall_s'])}]
    rows = []
    for entry in entries:
        label = run_label(entry['provenance'])
        if overture_release(entry) != overture_release(r):
            label += f" (different Overture release: {overture_release(entry) or 'unknown'})"
        wall = entry.get('wall_s')
        if wall is None:
            wall = statistics.median(stream_rows(entry)[0]['runs_wall_s'])
        rows.append((label, [wall / 60]))
    first, last = entries[0], entries[-1]
    lead = f"{minutes(rows[0][1][0] * 60)} at {run_label(first['provenance'])}"
    if len(entries) > 1:
        lead += (f" → {minutes(last['wall_s'])} at {run_label(last['provenance'])}, "
                 f"{factor(rows[0][1][0] * 60 / last['wall_s'])}× faster")
    return lead + '.\n\n' + bars("Africa wall time, shorter is faster", ["tylertoo"], rows, "min")


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
    return '\n'.join([*out, '</select>', '', table(['Dataset', 'tylertoo archive', 'tippecanoe archive'], rows)])


RENDERERS = {
    **{name: (lambda r, a, fn=fn: fn(r), False, True) for name, fn in {
        'speedup': speedup, 'speedup-bars': speedup_bars, 'memory-bars': memory_bars,
        'method': method, 'hero': hero, 'history': history, 'recipe': recipe,
        'encodings': encodings, 'sources': sources, 'profiles': profiles, 'compare-maps': compare_maps}.items()},
    'stamp': (lambda r, a: stamp(r), False, False),
    'readme-claim': (lambda r, a: readme_claim(r), True, False),
    'stream-claim': (lambda r, a: stream_claim(r), True, False),
}


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
    fn, inline, foot = RENDERERS[section]
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
        body = fn(results if section == 'stamp' else r, arg or None)
    if inline:
        return body
    return '\n' + body + ('\n\n' + footer(r) if foot and body != NOT_MEASURED else '') + '\n'


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
            page_stems = stems | ({'e2e-corpus', 'release-cells', 'overture-buildings-africa'} & results.keys()
                                  if any(m[1].endswith('/stamp') for m in matches) else set())
            page_warnings(root, rel, page_stems, results)
        new = rewrite(old, results)
        if new != old:
            stale.append(rel)
            if not check:
                path.write_text(new)
    if check:
        orphans = set(results) - used - {'comparison-pmtiles'}
        if orphans:
            raise SystemExit(f'orphan results JSON without a rendered region: {sorted(orphans)}')
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
