"""Generated benchmarks use recorded measurements and enforce page coverage."""

import copy
import json
import re
import shutil
import shlex
import subprocess
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import gen_bench_docs as g

REPO = g.ROOT
FIXTURES = Path(__file__).parent / "fixtures/bench"


@pytest.fixture
def results():
    return {p.stem: {**json.loads(p.read_text()), "_source_file": p.name}
            for p in FIXTURES.glob("*.json")}


def test_committed_docs_are_current():
    assert g.run(check=True) == []


def test_every_results_file_has_full_provenance():
    assert {"e2e-corpus", "release-cells", "overture-buildings-africa"} <= set(g.load_results())


def test_bars_scale_and_label_each_value():
    out = g.bars("Wall time", ["tylertoo", "tippecanoe"], [("A", [2, 8]), ("B", [4, 0])], "s")
    assert re.findall(r'--f:([\d.]+)', out) == ["0.250", "1.000", "0.500", "0.000"]
    assert out.index("tylertoo") < out.index("tippecanoe")
    assert out.count('aria-hidden="true"') == 4
    # Each value is printed at its bar's tip, so no table repeats the figures.
    assert '<details' not in out and '|' not in out
    assert '--f:0.250" aria-hidden="true"></span><span class="tt-val">2.0 s</span></span>' in out
    noted = g.bars("Wall time", ["tylertoo"], [("A", [2]), ("B", [4])], "s", notes={"A": "not in the mean"})
    assert noted.count("tt-excluded") == 1
    assert '<span class="tt-label">A <span class="tt-note">not in the mean</span></span>' in noted


def test_headline_leads_with_the_mean_and_memory(results):
    r = results["e2e-corpus"]
    out = g.headline(r)
    assert out.startswith("tylertoo was **20× faster than tippecanoe** across two GeoParquet-to-PMTiles builds.")
    assert "Both tools used the same peak memory on each dataset." in out
    for e in r["datasets"].values():
        e["tylertoo"]["peak_rss_bytes"] = 2 * e["tippecanoe_e2e"]["peak_rss_bytes"]
    assert "tippecanoe used less peak memory on each dataset." in g.headline(r)
    r["datasets"]["a"]["tylertoo"]["peak_rss_bytes"] = 1
    assert "tylertoo used less peak memory on one of three datasets." in g.headline(r)
    for e in r["datasets"].values():
        e["tylertoo"].pop("per_zoom", None)
    assert g.headline(r) == g.NOT_MEASURED


def test_speedup_geomean_uses_medians_and_equal_max_zoom(results):
    r = results["e2e-corpus"]
    n, included, excluded = g.speedup_geomean(r)
    assert n == pytest.approx(20)
    assert included == ["a", "b"]
    assert "zoom" in excluded["different-zoom"]
    r["datasets"]["different-zoom"]["tylertoo"]["per_zoom"]["by_zoom"] = {"9": {"tiles": 1}}
    assert "different-zoom" in g.speedup_geomean(r)[2]  # lower tippecanoe zoom also differs
    assert "20×" in g.readme_claim(r) and "20×" in g.headline(r)
    assert g.speedup(r).startswith("The 20× figure is the geometric mean across a and b.")


@pytest.mark.parametrize("missing", [None, "tylertoo", "tippecanoe_fgb", "both"])
def test_speedup_explains_the_recorded_exclusion(results, missing):
    r = results["e2e-corpus"]
    e = r["datasets"]["different-zoom"]
    for tool in ["tylertoo", "tippecanoe_fgb"]:
        if missing in (tool, "both"):
            e[tool].pop("per_zoom")
    reason = g.speedup_geomean(r)[2]["different-zoom"]
    if missing:
        assert reason == "no per-zoom tile counts were recorded"
    else:
        zooms = [g.zoom_top(e[t]) for t in ["tylertoo", "tippecanoe_fgb"]]
        assert reason == (f"the archives end at different zoom levels, "
                          f"z{zooms[0]} for tylertoo and z{zooms[1]} for tippecanoe")
    assert f"The different-zoom runs are shown separately because {reason}." in g.speedup(r)
    assert '<span class="tt-note">not in the mean</span>' in g.speedup_bars(r)


def test_excluded_rows_come_last_and_bars_name_the_speedup(results):
    r = results["e2e-corpus"]
    e = r["datasets"]["different-zoom"]
    ratio = e["tippecanoe_e2e"]["wall_median_s"] / e["tylertoo"]["wall_median_s"]
    for out in [g.speedup_bars(r), g.memory_bars(r)]:
        labels = re.findall(r'<span class="tt-label">([^<]+)', out)
        assert [label.strip() for label in labels] == ["a", "b", "different-zoom"]
    out = g.speedup_bars(r)
    assert f'{g.seconds(e["tylertoo"]["wall_median_s"])}, {g.factor(ratio)}× faster</span>' in out
    assert out.count('<span class="tt-note">not in the mean</span>') == len(g.speedup_geomean(r)[2])


def test_chart_captions(results):
    assert "<figcaption>Build time</figcaption>" in g.speedup_bars(results["e2e-corpus"])
    assert "<figcaption>Peak memory</figcaption>" in g.memory_bars(results["e2e-corpus"])


def test_chart_labels_drop_the_geometry_note():
    assert g.short_name("polygons-ftw-moldova-large") == "Moldova parcels"
    assert g.short_name("unknown-id") == "unknown-id"


def test_method_states_the_secondary_means(results):
    out = g.method(results["e2e-corpus"])
    assert "For tiling alone, without the conversion, the geometric mean is" in out
    qm = g.factor(g.statistics.geometric_mean([10 / 2, 40 / 2]))
    assert f"With thinning off (`--verbatim --simplify-factor 1.0`), the geometric mean is {qm}×." in out
    assert "That step is this share of it: a 10.0% and b 2.5%." in out
    assert "- different-zoom is not in the mean: the archives end at different zoom levels" in out


@pytest.mark.parametrize("value, expected", [(10, "10"), (23.4, "23"), (9.25, "9.2")])
def test_number_format(value, expected):
    assert g.factor(value) == expected


def test_label_and_footer(results):
    p = results["e2e-corpus"]["provenance"]
    assert g.run_label(p) == "commit abc1234"
    # A run past a release tag says plainly that it measured unreleased code.
    assert g.run_label({**p, "git_describe": "v0.8.0-52-gabcdef0"}) == "an unreleased commit abcdef0 after v0.8.0"
    assert g.run_label({**p, "git_describe": "v0.8.0-52-gabcdef0-dirty"}) == \
        "an unreleased commit abcdef0 after v0.8.0, with local changes"
    assert g.run_label({**p, "git_describe": "v0.8.0"}) == "v0.8.0"
    assert g.run_label({**p, "git_describe": "other", "release_version": "0.9.0"}) == "v0.9.0"
    assert g.run_label({**p, "release_version": "v0.9.0"}) == "v0.9.0"
    out = g.run_text(results["e2e-corpus"])
    for value in [p["commit"], p["allocator"], p["host"], p["date"][:10], *p["tool_versions"].values()]:
        assert value in out
    # A host string without memory gets it from mem_gib, once.
    assert g.host_text({"host": "aarch64, CPU 0xd40, 64 CPUs", "mem_gib": 246}) == "aarch64, CPU 0xd40, 64 CPUs, 246 GiB RAM"
    assert g.host_text({"host": "Neoverse-V1, 124 GB RAM", "mem_gib": 123}) == "Neoverse-V1, 124 GB RAM"


@pytest.mark.parametrize("host, mem, expected", [
    ("aarch64, CPU 0xd40, 64 CPUs", 247, "on an ARM64 host with 64 CPUs and 247 GiB of RAM"),
    ("x86_64, CPU Xeon, 8 CPUs, 32 GiB RAM", None, "on an x86-64 host with 8 CPUs and 32 GiB of RAM"),
    ("aarch64, CPU 0xd40, 64 CPUs", None, "on an ARM64 host with 64 CPUs"),
    ("Test host", None, "on Test host"),
])
def test_host_prose(host, mem, expected):
    assert g.host_prose({"host": host, "mem_gib": mem}) == expected


def test_long_date():
    assert g.long_date({"date": "2026-11-27T15:51:05Z"}) == "November 27, 2026"
    assert g.long_date({"date": None}) == "an unrecorded date"


def test_runs_state_a_shared_build_once(results):
    out = g.runs(results)
    assert out.count("abc1234") == 1
    assert out.startswith("Every run used tylertoo commit abc1234, built with `mimalloc`, on Test host, on 2026-11-01.")
    for stem, label in g.PAGE_FILES.items():
        assert f"| {label} | " in out and f"[`{stem}.json`]({g.RESULTS_URL}{stem}.json)" in out
    results["release-cells"]["provenance"]["commit"] = "def5678"
    out = g.runs(results)
    assert "Every run" not in out and "| Memory profiles | commit def5678 |" in out


def test_figures_hold_every_recorded_number(results):
    r = results["e2e-corpus"]
    r["datasets"]["a"]["tylertoo"]["output_bytes"] = 61654743
    out = g.figures(r)
    header, rule, *rows = out.splitlines()
    assert "tippecanoe tiling only" in header and "Speedup, thinning off" in header
    assert rule == "|---|" + "---:|" * 9
    assert rows[-1].startswith("| different-zoom, not in the mean |")
    assert "61.7 MB" in out


def test_charts_carry_no_provenance(results):
    for region in ["e2e-corpus/speedup-bars", "e2e-corpus/memory-bars", "e2e-corpus/figures",
                   "overture-buildings-africa/hero", "release-cells/profiles"]:
        out = g.render_region(region, results)
        assert "Run:" not in out and "abc1234" not in out


@pytest.mark.parametrize("raw, version", [
    ("geoparquet-io, version 1.7.0", "1.7.0"),
    ("geoparquet-io, version 12.34.56rc1", "12.34.56rc1"),
    ("1.7.0", "1.7.0"),
    ("unknown development build", "unknown development build"),
    ("geoparquet-io, version unknown", "geoparquet-io, version unknown"),
])
def test_gpio_version_in_runs_and_footer(results, raw, version):
    r = results["e2e-corpus"]
    r["provenance"]["tool_versions"]["gpio"] = raw
    for out in [g.run_text(r), g.runs(results)]:
        assert f"gpio {version}" in out
        if version != raw:
            assert raw not in out
            assert re.search(r"gpio (\d+\.\d+\.\d+\w*)", out)[1] == raw.split()[-1]


def test_recipe_uses_recorded_command(results):
    out = g.recipe(results["overture-buildings-africa"])
    assert out == ("The run used Overture release `current` and processed 200 features.\n\n"
                   "```bash\nAWS_SKIP_SIGNATURE=true \\\ntylertoo tiles \\\n  s3://bucket/prefix \\\n"
                   "  africa.pmtiles \\\n  --max-zoom 14\n```")
    r = results["overture-buildings-africa"]
    r["rows"][0].pop("argv")
    assert g.recipe(r) == g.NOT_MEASURED


def test_command_lines_keep_flag_values_and_equals_forms():
    out = g.command_lines([], ["tylertoo", "tiles", "in", "out", "--a", "1", "--b=2", "--c"])
    assert out == "tylertoo tiles \\\n  in \\\n  out \\\n  --a 1 \\\n  --b=2 \\\n  --c"
    assert shlex.split(out.replace("\\\n", " ")) == ["tylertoo", "tiles", "in", "out", "--a", "1", "--b=2", "--c"]


def test_hero_same_archive_only_when_recorded(results):
    r = results["overture-buildings-africa"]
    assert "--profile bounded" in g.hero(r) and "same archive" in g.hero(r)
    r["rows"][1]["archive_sha256"] = ["different"]
    assert "same archive" not in g.hero(r)
    r["rows"] = r["rows"][:1]
    assert "--profile bounded" not in g.hero(r)


def test_hero_stats_are_one_block(results):
    r = results["overture-buildings-africa"]
    block = re.search(r'^<div class="tt-stats">\n(.*?)\n</div>', g.hero(r), re.S)
    tiles = re.findall(r'<span class="tt-stat-value">(.*?)</span> <span class="tt-stat-label">(.*?)</span>', block[1])
    assert tiles == [(g.minutes(g.statistics.median(r['rows'][0]['runs_wall_s'])), "build time"),
                     (g.millions(r['features']), "buildings"),
                     (g.gb(r['rows'][0]['fetched_bytes']), "fetched from S3"),
                     (g.gb(r['rows'][0]['peak_rss_bytes']), "peak memory")]


def test_profiles_both_shapes(results):
    r = results["release-cells"]
    expected = g.profiles(r)
    assert expected.startswith("The `bounded` profile uses less peak memory and takes longer to build. On ")
    assert "reduced peak memory from 1.0 GiB to 0.5 GiB and increased build time from 10.0 s to 20.0 s." in expected
    for label in ["Madagascar admin boundaries", "Moldova parcels", "New Zealand parcels"]:
        assert expected.count(f'class="tt-label">{label}</span>') == 2
    for cell, bins in list(r["cells"].items()):
        r["cells"][cell] = {"release": bins["auto"]}
        r["cells"][cell + "-bounded"] = {"release": bins["bounded"]}
    assert g.profiles(r) == expected


def test_profiles_lead_follows_mixed_results(results):
    r = results["release-cells"]
    r["cells"]["polygons"]["bounded"]["true_peak_mib"]["median"] = 4096
    assert g.profiles(r).startswith(
        "The `bounded` profile used less peak memory on two of three datasets and took longer on three.")


@pytest.mark.parametrize("section", ["profiles", "compare-maps", "hero", "history", "recipe", "method", "figures", "runs"])
def test_missing_measurements(section):
    out = g.render_region(f"missing/{section}", {})
    assert out.strip() == g.NOT_MEASURED and "Run:" not in out
    if section == "profiles":
        assert g.RENDERERS[section][0]({"cells": {}}, None) == g.NOT_MEASURED


def comparison(results):
    base = "https://data.source.coop/nlebovits/tylertoo-benchmarks/0.9.0/"
    return {"provenance": results["e2e-corpus"]["provenance"], "version": "0.9.0", "base_url": base,
            "datasets": [{"id": "a", "geometry": "point", "layer": "places", **{tool: {"url": f"{base}a/{tool}.pmtiles"} for tool in ["tylertoo", "tippecanoe"]}}]}


def test_compare_maps(results):
    r = comparison(results)
    results["comparison-pmtiles"] = r
    out = g.render_region("comparison-pmtiles/compare-maps", results)
    for needle in ['<label', '<select id="compare-dataset"', 'data-tylertoo=', 'data-tippecanoe=', 'data-layer="places"', 'data-geometry="point"', '| Dataset | tylertoo archive | Tippecanoe archive |']:
        assert needle in out
    r["datasets"][0]["tylertoo"]["url"] += "?bad"
    with pytest.raises(ValueError, match="URL"):
        g.render_region("comparison-pmtiles/compare-maps", results)


@pytest.mark.parametrize("change", ["version", "id"])
def test_compare_maps_cross_checks_corpus(results, change):
    r = comparison(results)
    results["comparison-pmtiles"] = r
    if change == "version":
        r["provenance"] = {**r["provenance"], "tylertoo_version": "other"}
    else:
        r["datasets"][0]["id"] = "unknown"
    with pytest.raises(ValueError):
        g.render_region("comparison-pmtiles/compare-maps", results)


def setup_page(tmp_path, results):
    d = tmp_path / "benchmarks/results"
    d.mkdir(parents=True)
    for name, r in results.items():
        (d / f"{name}.json").write_text(json.dumps(r))
    doc = tmp_path / "docs/benchmarks.md"
    doc.parent.mkdir()
    doc.write_text("# Benchmarks\n<!-- bench:e2e-corpus/runs --><!-- /bench -->\n<!-- bench:overture-buildings-africa/hero --><!-- /bench -->\n<!-- bench:release-cells/profiles --><!-- /bench -->\n")
    g.run(root=tmp_path, results_dir=d, files=["docs/benchmarks.md"])
    return doc, d


def test_check_catches_hand_edit(tmp_path, results):
    doc, d = setup_page(tmp_path, results)
    doc.write_text(doc.read_text().replace("abc1234", "abc1235", 1))
    assert g.run(root=tmp_path, results_dir=d, check=True, files=["docs/benchmarks.md"]) == ["docs/benchmarks.md"]


def test_check_validates_unlisted_comparison_manifest(tmp_path, results):
    """The compare-maps page may be absent; --check still validates its manifest."""
    bad = comparison(results)
    bad["datasets"][0]["tippecanoe"]["url"] += "?bad"
    _, d = setup_page(tmp_path, {**results, "comparison-pmtiles": bad})
    with pytest.raises(ValueError, match="URL"):
        g.run(root=tmp_path, results_dir=d, check=True, files=["docs/benchmarks.md"])


@pytest.mark.parametrize("problem", ["digit", "orphan", "lines", "coverage", "missing", "marker"])
def test_check_invariants(tmp_path, results, problem):
    doc, d = setup_page(tmp_path, results)
    expected = {"digit": "digit", "orphan": "orphan", "lines": str(getattr(g, "PROSE_LINE_CAP", 40)), "coverage": "DOC_FILES", "missing": "missing", "marker": "marker"}[problem]
    if problem == "digit":
        doc.write_text(doc.read_text() + "\nTakes 42 seconds.\n")
    elif problem == "orphan":
        shutil.copy(d / "e2e-corpus.json", d / "orphan.json")
    elif problem == "lines":
        doc.write_text(doc.read_text() + "\nExtra prose.\n" * g.PROSE_LINE_CAP)
    elif problem == "coverage":
        (tmp_path / "forgotten.md").write_text("<!-- bench:e2e-corpus/speedup --><!-- /bench -->")
    elif problem == "missing":
        doc.unlink()
    else:
        doc.write_text("# Benchmarks\n")
    with pytest.raises(SystemExit, match=expected):
        g.run(root=tmp_path, results_dir=d, check=True, files=["docs/benchmarks.md"])


def test_prose_strips_code_comments_links_and_s3():
    assert g.prose_errors('# S3 {#from-s3}\n`42`\n```bash\n123\n```\n<!-- 55 -->\n[link](v2/3)') == []
    assert g.prose_errors("\n".join(["Words"] * g.PROSE_LINE_CAP)) == []
    assert str(g.PROSE_LINE_CAP) in g.prose_errors("\n".join(["Words"] * (g.PROSE_LINE_CAP + 1)))[0]


def test_version_warnings_do_not_fail(tmp_path, results, capsys):
    doc, d = setup_page(tmp_path, results)
    (tmp_path / "Cargo.toml").write_text('[workspace.package]\nversion = "99.0.0"\n')
    assert g.run(root=tmp_path, results_dir=d, check=True, files=["docs/benchmarks.md"]) == []
    out = capsys.readouterr().out
    assert "::warning::" in out and "measured at 0.9.0; this branch is 99.0.0; run benchmarks/release.sh" in out
    r = results["release-cells"]
    r["provenance"]["tylertoo_version"] = "0.8.0"
    (d / "release-cells.json").write_text(json.dumps(r))
    g.run(root=tmp_path, results_dir=d, files=["docs/benchmarks.md"])
    assert g.run(root=tmp_path, results_dir=d, check=True, files=["docs/benchmarks.md"]) == []
    assert "mixes versions" in capsys.readouterr().out


def test_git_unavailable_coverage_skips_build_trees(tmp_path, monkeypatch):
    def unavailable(*a, **kw):
        raise FileNotFoundError
    monkeypatch.setattr(g.subprocess, "run", unavailable)
    for folder in ["target", ".git", "node_modules", "context"]:
        path = tmp_path / folder / "test.md"
        path.parent.mkdir()
        path.write_text("<!-- bench:missing/hero -->")
    (tmp_path / "yes.md").write_text("<!-- bench:missing/hero -->")
    assert g.marker_files(tmp_path) == {"yes.md"}


def test_diff_ranks_whole_wall_and_skips_unknown_hosts(results, monkeypatch, capsys):
    old = copy.deepcopy(results)
    for r in [*results.values(), *old.values()]:
        r["provenance"]["host_class"] = "same"
        r["provenance"]["mem_gib"] = 247
    old["e2e-corpus"]["datasets"]["a"]["tylertoo"]["wall_median_s"] = 100
    results["e2e-corpus"]["datasets"]["b"]["tylertoo"]["wall_median_s"] = 100
    old["release-cells"]["provenance"].pop("host_class")
    old["overture-buildings-africa"]["provenance"]["host_class"] = "another"
    def show(argv, **kw):
        assert argv[:2] == ["git", "show"]
        return subprocess.CompletedProcess(argv, 0, json.dumps(old[Path(argv[2]).stem]), "")
    monkeypatch.setattr(g.subprocess, "run", show)
    g.diff_results("base", results, {"e2e-corpus": {"speedup-bars", "memory-bars"}, "release-cells": {"profiles"}, "overture-buildings-africa": {"hero"}})
    out = capsys.readouterr().out
    assert out.index("b/tylertoo") < out.index("a/tylertoo")
    assert "100.0 s → 1.0 s" in out
    assert "unknown host_class" in out and "different host_class" in out
    assert "CPU" not in out and "phase" not in out


@pytest.mark.parametrize('change', ['host', 'memory', 'unknown-memory', 'unknown-host'])
def test_history_separates_incomparable_hosts(results, change):
    r = results['overture-buildings-africa']
    r['provenance'].update(host_class='same', mem_gib=247, host='Current host')
    old = r['history'][0]['provenance']
    old.update(host_class='same', mem_gib=247, host='Previous host')
    if change == 'host':
        old['host_class'] = 'other'
    elif change == 'memory':
        old['mem_gib'] = 123
    elif change == 'unknown-memory':
        old.pop('mem_gib')
    else:
        old.pop('host_class')
    out = g.history(r)
    # A lone current run needs no chart; the other host's run is a table row.
    assert '<figure' not in out
    assert '→' not in out and '× faster' not in out
    assert 'Current host' not in out and 'Previous host' in out
    row = re.search(r'^\| commit old1234 \| (.*?) \| (.*?) \| (.*?) \|$', out, re.M)
    assert row and 'Previous host' in row[1] and row[3] == g.minutes(r['history'][0]['wall_s'])
    if change == 'unknown-memory':
        assert 'memory not recorded' in row[1]


def test_history_derives_legacy_host_class(results):
    r = results['overture-buildings-africa']
    r['provenance'].update(host_class='aarch64/0xd40/64', mem_gib=247)
    old = r['history'][0]['provenance']
    old.update(host='aarch64, CPU 0xd40, 64 CPUs', mem_gib=247)
    old.pop('host_class', None)
    out = g.history(r)
    assert out.count('<figure') == 1 and '→' in out


def test_committed_history_charts_only_comparable_runs():
    r = g.load_results()['overture-buildings-africa']
    current = g.history_host(r['provenance'])
    same = [e for e in r.get('history', []) if current and g.history_host(e['provenance']) == current]
    out = g.history(r)
    assert out.count('<figure') == (1 if same else 0)
    for e in r.get('history', []):
        if e not in same:
            assert f"| {g.run_label(e['provenance'])} |" in out


def test_diff_skips_memory_change(results, monkeypatch, capsys):
    r = results['e2e-corpus']
    r['provenance'].update(host_class='same', mem_gib=247)
    old = copy.deepcopy(r)
    old['provenance']['mem_gib'] = 123
    monkeypatch.setattr(g.subprocess, 'run', lambda *a, **kw:
                        subprocess.CompletedProcess([], 0, json.dumps(old), ''))
    g.diff_results('base', results, {'e2e-corpus': {'speedup-bars'}})
    out = capsys.readouterr().out
    assert 'mem_gib' in out and 'skipped' in out and 'a/tylertoo' not in out


@pytest.mark.parametrize('actual', [857315, 42, None])
def test_tutorial_tile_count_check(tmp_path, results, capsys, actual):
    doc, d = setup_page(tmp_path, results)
    readme = tmp_path / 'examples/brazil-fields/README.md'
    readme.parent.mkdir(parents=True)
    readme.write_text('''Other output: 12 tiles across z0..z14
```text
999 tiles across z0..z14
```
File: `01-tiles.sh`

```bash
tylertoo tiles input brazil.pmtiles
```
```text
✓ Converted input → brazil.pmtiles
  857,315 tiles across z0..z14 in 174.16s
```
```text
321 tiles across z0..z14
```
''')
    if actual is not None:
        (d / 'tutorial-map.json').write_text(json.dumps(
            {'provenance': results['e2e-corpus']['provenance'], 'archive': {'tiles': actual}}))
    if actual == 42:
        with pytest.raises(SystemExit) as exc:
            g.run(root=tmp_path, results_dir=d, check=True, files=['docs/benchmarks.md'])
        message = str(exc.value)
        assert '42' in message and '857,315' in message
        assert 're-run benchmarks/release.sh tutorial-map docs' in message
        assert 'update the tutorial' in message
        # Stage names in the advice must exist.
        stages = re.search(r'^STAGES=\((.*?)\)', (REPO / 'benchmarks/release.sh').read_text(), re.M)[1].split()
        assert all(s in stages for s in ['tutorial-map', 'docs'])
    else:
        assert g.run(root=tmp_path, results_dir=d, check=True, files=['docs/benchmarks.md']) == []
        if actual is None:
            assert '::warning::' in capsys.readouterr().out


def test_tutorial_parser_does_not_borrow_another_steps_output(tmp_path, results):
    _, d = setup_page(tmp_path, results)
    path = tmp_path / 'examples/brazil-fields/README.md'
    path.parent.mkdir(parents=True)
    path.write_text('''File: `01-tiles.sh`

```bash
tylertoo tiles input brazil.pmtiles
```
The expected output is missing.

File: `02-shard.sh`

```bash
tylertoo merge parts output.pmtiles
```
```text
42 tiles across z0..z14
```
''')
    (d / 'tutorial-map.json').write_text(json.dumps(
        {'provenance': results['e2e-corpus']['provenance'], 'archive': {'tiles': 42}}))
    with pytest.raises(SystemExit, match='step-one expected tile count missing'):
        g.run(root=tmp_path, results_dir=d, check=True, files=['docs/benchmarks.md'])


def test_unknown_region_is_an_error():
    with pytest.raises(SystemExit):
        g.rewrite("<!-- bench:nope/no-such-renderer --><!-- /bench -->", {})


def test_method_only_claims_recorded_settings(results):
    r = results['e2e-corpus']
    assert 'warm-up' not in g.method(r)
    assert "- Each time is the median of three runs with a warm cache." in g.method(r)
    r['settings']['warmup'] = 2
    assert "after two discarded warm-up runs" in g.method(r)
    r['settings'].pop('warm_cache')
    assert 'cache' not in g.method(r)
    r['settings'].pop('repeat')
    assert "median of the recorded runs" in g.method(r)
    r['settings']['alternation'] = 'interleaved'
    assert "- Run order: interleaved." in g.method(r)


def test_method_keeps_commands_in_one_disclosure_grouped_by_dataset():
    r = g.load_results()["e2e-corpus"]
    out = g.method(r)
    opening = "<details markdown><summary>Exact commands for tylertoo vs. tippecanoe</summary>"
    assert out.count(opening) == 1 and out.count("</details>") == 1
    short, commands = out.split(opening)
    assert f"Input checksums are pinned in `{r['_source_file']}`." in short
    assert f"median of {g.words(r['settings']['repeat'])} runs" in short
    assert "command:" not in short
    assert all(pin["sha256"] not in out for pin in r["provenance"]["inputs"])
    groups = re.findall(r"^- (.+):\n((?:    - .+\n?)+)", commands, re.M)
    assert [name for name, _ in groups] == [g.DISPLAY.get(did, did) for did in r["datasets"]]
    for (_, group), e in zip(groups, r["datasets"].values()):
        recorded = [e[tool]["argv"] for tool in
                    ["tylertoo", "tippecanoe_fgb", "convert_fgb", "tylertoo_quality_matched"]
                    if e.get(tool, {}).get("argv")]
        assert [shlex.split(argv) for argv in re.findall(r"`([^`]+)`", group)] == recorded


def test_method_omits_disclosure_without_recorded_commands(results):
    assert "<details" not in g.method(results["e2e-corpus"])


def test_rendered_values_match_measurements(results):
    r = results['overture-buildings-africa']
    stat = dict((label, value) for value, label in re.findall(
        r'<span class="tt-stat-value">(.*?)</span> <span class="tt-stat-label">(.*?)</span>', g.hero(r)))
    assert int(re.search(r'([\d,]+) features', g.recipe(r))[1].replace(',', '')) == r['features']
    assert g.millions(526_532_679) == "527 million" and g.millions(200) == "200"
    assert float(stat['build time'].split()[0].replace(',', '')) == pytest.approx(
        g.statistics.median(r['rows'][0]['runs_wall_s']) / 60, abs=0.5)
    for label, key in [('peak memory', 'peak_rss_bytes'), ('fetched from S3', 'fetched_bytes')]:
        assert float(stat[label].split()[0].replace(',', '')) == pytest.approx(r['rows'][0][key] / 1e9, abs=0.05)
    corpus = results['e2e-corpus']
    assert g.readme_claim(corpus) == f"{g.factor(g.speedup_geomean(corpus)[0])}× faster than tippecanoe"


def test_recorded_africa_recipe_flags_and_link():
    import tomllib
    r = g.load_results()['overture-buildings-africa']
    manifest = tomllib.loads((REPO / 'benchmarks/showcase/manifest-africa.toml').read_text())
    cli = (REPO / 'docs/reference/cli.md').read_text()
    row, _ = g.stream_rows(r)
    spec = manifest['cells']['ovt-s3']
    assert row['argv'] == ['tylertoo', spec['command'], *spec['args'],
                           '--min-zoom', str(spec['zoom'][0]), '--max-zoom', str(spec['zoom'][1]), *spec['flags']]
    assert row['env'] == spec['env']
    # The page also retains the recorded bounded run from before that cell was retired.
    for row in r['rows']:
        for flag in re.findall(r'--[a-z][a-z-]+', ' '.join(row['argv'])):
            assert flag in cli
    # The heading names buildings; its anchor keeps the links in the READMEs working.
    assert '## Streaming Overture buildings from S3 {#streaming-overture-from-s3}' in (REPO / 'docs/benchmarks.md').read_text()
    for rel in g.DOC_FILES[:4]:
        assert '#streaming-overture-from-s3' in (REPO / rel).read_text()


def test_check_accepts_not_measured_files(tmp_path):
    doc = tmp_path / 'a.md'
    d = tmp_path / 'results'
    d.mkdir()
    doc.write_text('<!-- bench:release-cells/profiles --><!-- /bench -->')
    g.run(root=tmp_path, results_dir=d, files=['a.md'])
    assert g.NOT_MEASURED in doc.read_text()
    assert g.run(root=tmp_path, results_dir=d, check=True, files=['a.md']) == []


def test_recipe_replaces_actual_run_output_path(results):
    r = results['overture-buildings-africa']
    r['rows'][0]['argv'][3] = '/bench/scratch/africa-s3/ovt-s3/head-r0/out.pmtiles'
    assert 'africa.pmtiles' in g.recipe(r)
    assert '/bench/scratch/' not in g.recipe(r)


def test_readme_claims_carry_no_version_or_date(results):
    for r in results.values():
        r["provenance"].update(release_version="0.9.0", git_describe="v0.8.0-52-gabcdef0")
    claims = [g.readme_claim(results["e2e-corpus"]), g.stream_claim(results["overture-buildings-africa"])]
    for claim in claims:
        assert "0.9.0" not in claim and "abc1234" not in claim and "2026" not in claim
        assert not claim.endswith(")")


def test_stream_claim_text(results):
    r = results["overture-buildings-africa"]
    r.update(features=526_532_679, in_scope_bytes=58.7e9)
    assert g.stream_claim(r) == ("Capable of streaming 527 million Overture buildings (58.7 GB) "
                                 "from S3 into PMTiles in 27 minutes")
