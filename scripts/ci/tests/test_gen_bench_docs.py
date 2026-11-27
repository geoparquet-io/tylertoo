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


def test_bars_scale_and_accessible_table():
    out = g.bars("Wall time", ["tylertoo", "tippecanoe"], [("A", [2, 8]), ("B", [4, 0])], "s")
    assert re.findall(r'--w:([\d.]+)%', out) == ["25.0", "100.0", "50.0", "0.0"]
    assert out.index("tylertoo") < out.index("tippecanoe")
    assert out.count('aria-hidden="true"') == 4
    assert '<details markdown>' in out and '| A | 2.0 s | 8.0 s |' in out


def test_speedup_geomean_uses_medians_and_equal_max_zoom(results):
    r = results["e2e-corpus"]
    n, included, excluded = g.speedup_geomean(r)
    assert n == pytest.approx(20)
    assert included == ["a", "b"]
    assert "zoom" in excluded["different-zoom"]
    r["datasets"]["different-zoom"]["tylertoo"]["per_zoom"]["by_zoom"] = {"9": {"tiles": 1}}
    assert "different-zoom" in g.speedup_geomean(r)[2]  # lower tippecanoe zoom also differs
    assert "20×" in g.readme_claim(r) and "**20×**" in g.speedup(r)
    for phrase in ["Included", "Excluded", "conversion", "tiling alone", "thinning off"]:
        assert phrase.lower() in g.speedup(r).lower()


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
        assert "100× faster (no per-zoom tile counts recorded)" in g.speedup_bars(r)
    else:
        zooms = [g.zoom_top(e[t]) for t in ["tylertoo", "tippecanoe_fgb"]]
        assert reason == (f"the archives reach different max zooms "
                          f"(tylertoo z{zooms[0]}, tippecanoe z{zooms[1]})")
        assert list(map(int, re.findall(r"z(\d+)", reason))) == zooms
    assert reason in g.speedup(r)


def test_speedup_labels_put_faster_before_the_caveat(results):
    r = results["e2e-corpus"]
    e = r["datasets"]["different-zoom"]
    ratio = e["tippecanoe_e2e"]["wall_median_s"] / e["tylertoo"]["wall_median_s"]
    out = g.speedup_bars(r)
    expected = f"{g.factor(ratio)}× faster (max zoom differs)"
    assert f'{g.seconds(e["tylertoo"]["wall_median_s"])}, {expected}</span>' in out
    assert f"| {expected} |" in out
    assert float(re.search(r"([\d.]+)× faster \(max zoom differs\)", out)[1]) == ratio


def test_memory_caption(results):
    assert "<figcaption>Peak memory, shorter is less</figcaption>" in g.memory_bars(results["e2e-corpus"])


def test_speedup_uses_plain_metric_labels(results):
    r = results["e2e-corpus"]
    assert "GeoParquet to FlatGeobuf conversion as a share of tippecanoe's total wall time:" in g.speedup(r)
    assert "For tiling alone, the geometric mean is" in g.speedup(r)
    out = g.speedup_bars(r)
    assert "<figcaption>End-to-end wall time, shorter is faster</figcaption>" in out
    assert "| tippecanoe tiling only |" in out


@pytest.mark.parametrize("value, expected", [(10, "10"), (23.4, "23"), (9.25, "9.2")])
def test_number_format(value, expected):
    assert g.factor(value) == expected


def test_label_and_footer(results):
    p = results["e2e-corpus"]["provenance"]
    assert g.run_label(p) == "commit abc1234"
    assert g.run_label({**p, "git_describe": "v0.8-2-gabc"}) == "v0.8-2-gabc"
    assert g.run_label({**p, "git_describe": "other", "release_version": "0.9.0"}) == "0.9.0"
    out = g.footer(results["e2e-corpus"])
    for value in [p["commit"], p["allocator"], p["host"], p["date"][:10], *p["tool_versions"].values()]:
        assert value in out


def test_stamp_lists_page_files(results):
    out = g.stamp(results)
    for r in results.values():
        if "comparison" not in r.get("_source_file", ""):
            assert g.run_label(r["provenance"]) in out
    assert "Corpus:" in out and "Africa:" in out and "Release cells:" in out


@pytest.mark.parametrize("raw, version", [
    ("geoparquet-io, version 1.7.0", "1.7.0"),
    ("geoparquet-io, version 12.34.56rc1", "12.34.56rc1"),
    ("1.7.0", "1.7.0"),
    ("unknown development build", "unknown development build"),
    ("geoparquet-io, version unknown", "geoparquet-io, version unknown"),
])
def test_gpio_version_in_stamp_and_footer(results, raw, version):
    r = results["e2e-corpus"]
    r["provenance"]["tool_versions"]["gpio"] = raw
    for out in [g.footer(r), g.stamp(results)]:
        assert f"gpio {version}," in out or f"gpio {version}." in out
        if version != raw:
            assert raw not in out
            assert re.search(r"gpio[: ]+(\d+\.\d+\.\d+\w*)", out)[1] == raw.split()[-1]


def test_history_line_and_release_change(results):
    r = results["overture-buildings-africa"]
    out = g.history(r)
    assert "54 min at commit old1234 → 27 min at commit abc1234, 2.0× faster" in out
    assert "different Overture release" in out
    assert out.count('class="tt-bar tt-a"') == 2
    r["history"] = []
    assert "→" not in g.history(r)


def test_recipe_uses_recorded_command(results):
    out = g.recipe(results["overture-buildings-africa"])
    assert "```bash\nAWS_SKIP_SIGNATURE=true tylertoo tiles s3://bucket/prefix africa.pmtiles --max-zoom 14\n```" == out
    r = results["overture-buildings-africa"]
    r["rows"][0].pop("argv")
    assert g.recipe(r) == g.NOT_MEASURED


def test_hero_same_archive_only_when_recorded(results):
    r = results["overture-buildings-africa"]
    assert 'class="tt-hero"' in g.hero(r)
    assert "--profile bounded" in g.hero(r) and "same archive" in g.hero(r)
    r["rows"][1]["archive_sha256"] = ["different"]
    assert "same archive" not in g.hero(r)
    r["rows"] = r["rows"][:1]
    assert "--profile bounded" not in g.hero(r)


def test_hero_is_one_markdown_block(results):
    r = results["overture-buildings-africa"]
    out = g.hero(r)
    block = re.search(r'^<div class="tt-hero" markdown>\n\n(.*?)\n\n</div>', out, re.S)
    assert block
    values = [f"{r['features']:,} features",
              f"{g.minutes(g.statistics.median(r['rows'][0]['runs_wall_s']))} wall time",
              f"{g.gb(r['rows'][0]['peak_rss_bytes'])} peak memory",
              f"{g.gb(r['rows'][0]['fetched_bytes'])} fetched"]
    for value in values:
        assert value in block[1]
    assert out.count(". ".join(values) + ".") == 1


def test_profiles_both_shapes(results):
    r = results["release-cells"]
    expected = g.profiles(r)
    for label in ["Madagascar admin boundaries", "Moldova parcels", "New Zealand parcels"]:
        assert expected.count(f'class="tt-label">{label}</span>') == 2
        assert expected.count(f"| {label} |") == 2
    for cell, bins in list(r["cells"].items()):
        r["cells"][cell] = {"release": bins["auto"]}
        r["cells"][cell + "-bounded"] = {"release": bins["bounded"]}
    assert g.profiles(r) == expected


@pytest.mark.parametrize("section", ["encodings", "sources", "profiles", "compare-maps", "hero", "history", "recipe"])
def test_missing_measurements(section):
    out = g.render_region(f"missing/{section}", {})
    assert out.strip() == g.NOT_MEASURED and "Run:" not in out
    if section in ["encodings", "sources", "profiles"]:
        assert g.RENDERERS[section][0]({"cells": {}}, None) == g.NOT_MEASURED


def test_input_cells(results):
    r = results["release-cells"]
    sample = copy.deepcopy(r["cells"]["polygons"])
    r["cells"].update({key: sample for key in ["in-geoarrow", "in-geo2", "in-geo-only", "in-parts13", "in-dir13", "in-http13"]})
    assert "native GeoArrow" in g.encodings(r)
    out = g.sources(r)
    assert "HTTPS" in out and "directory" in out and "#streaming-overture-from-s3" in out


def comparison(results):
    base = "https://data.source.coop/nlebovits/tylertoo-benchmarks/0.9.0/"
    return {"provenance": results["e2e-corpus"]["provenance"], "version": "0.9.0", "base_url": base,
            "datasets": [{"id": "a", "geometry": "point", "layer": "places", **{tool: {"url": f"{base}a/{tool}.pmtiles"} for tool in ["tylertoo", "tippecanoe"]}}]}


def test_compare_maps(results):
    r = comparison(results)
    results["comparison-pmtiles"] = r
    out = g.render_region("comparison-pmtiles/compare-maps", results)
    for needle in ['<label', '<select id="compare-dataset"', 'data-tylertoo=', 'data-tippecanoe=', 'data-layer="places"', 'data-geometry="point"', '| Dataset | tylertoo archive | tippecanoe archive |']:
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
    doc.write_text("# Benchmarks\n<!-- bench:e2e-corpus/stamp --><!-- /bench -->\n<!-- bench:overture-buildings-africa/hero --><!-- /bench -->\n<!-- bench:release-cells/profiles --><!-- /bench -->\n")
    g.run(root=tmp_path, results_dir=d, files=["docs/benchmarks.md"])
    return doc, d


def test_check_catches_hand_edit(tmp_path, results):
    doc, d = setup_page(tmp_path, results)
    doc.write_text(doc.read_text().replace("abc1234", "abc1235", 1))
    assert g.run(root=tmp_path, results_dir=d, check=True, files=["docs/benchmarks.md"]) == ["docs/benchmarks.md"]


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
    assert g.prose_errors('# S3\n`42`\n```bash\n123\n```\n<!-- 55 -->\n[link](v2/3)') == []
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


def test_unknown_region_is_an_error():
    with pytest.raises(SystemExit):
        g.rewrite("<!-- bench:nope/headline --><!-- /bench -->", {})


def test_method_only_claims_recorded_settings(results):
    r = results['e2e-corpus']
    assert 'warm-up' not in g.method(r)
    r['settings']['warmup'] = 2
    assert f"Discarded warm-ups: {r['settings']['warmup']}" in g.method(r)
    r['settings'].pop('warm_cache')
    assert 'Warm cache' not in g.method(r)


def test_method_keeps_commands_in_one_disclosure_grouped_by_dataset():
    r = g.load_results()["e2e-corpus"]
    out = g.method(r)
    opening = "<details markdown><summary>Exact commands</summary>"
    assert out.count(opening) == 1 and out.count("</details>") == 1
    short, commands = out.split(opening)
    assert f"- Inputs: pinned by sha256 in `{r['_source_file']}`." in short
    assert f"- Allocator: {r['provenance']['allocator']}." in short
    assert int(re.search(r"- Rounds: (\d+)\.", short)[1]) == r["settings"]["repeat"]
    assert "command:" not in short and "Input checksum" not in out
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
    out = g.hero(r)
    values = re.search(r'([\d,]+) features\. ([\d,]+) min wall time\. '
                       r'([\d,.]+) GB peak memory\. ([\d,.]+) GB fetched\.', out)
    assert int(values[1].replace(',', '')) == r['features']
    assert float(values[2].replace(',', '')) == pytest.approx(
        g.statistics.median(r['rows'][0]['runs_wall_s']) / 60, abs=0.5)
    for observed, key in zip(values.group(3, 4), ['peak_rss_bytes', 'fetched_bytes']):
        assert float(observed.replace(',', '')) == pytest.approx(r['rows'][0][key] / 1e9, abs=0.05)
    corpus = results['e2e-corpus']
    assert re.search(r'(\d+) datasets', g.readme_claim(corpus))[1] == str(len(g.speedup_geomean(corpus)[1]))


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
    assert '## Streaming Overture from S3' in (REPO / 'docs/benchmarks.md').read_text()
    for rel in g.DOC_FILES[:4]:
        assert '#streaming-overture-from-s3' in (REPO / rel).read_text()


def test_check_accepts_not_measured_files(tmp_path):
    doc = tmp_path / 'a.md'
    d = tmp_path / 'results'
    d.mkdir()
    doc.write_text('<!-- bench:release-cells/encodings --><!-- /bench -->')
    g.run(root=tmp_path, results_dir=d, files=['a.md'])
    assert g.NOT_MEASURED in doc.read_text()
    assert g.run(root=tmp_path, results_dir=d, check=True, files=['a.md']) == []


def test_archive_sizes_keep_useful_precision(results):
    r = results['e2e-corpus']
    r['datasets']['a']['tylertoo']['output_bytes'] = 61654743
    assert '61.7 MB' in g.speedup_bars(r)
