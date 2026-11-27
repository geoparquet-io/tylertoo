"""
Tests for run_manifest.py and the manifest it reads (#734).

    uv run --no-project --with pytest pytest benchmarks/test_run_manifest.py
"""

from __future__ import annotations

import importlib.util
import io
import json
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import run_manifest as rm  # noqa: E402


@pytest.fixture(scope="module")
def manifest() -> dict:
    return rm.load_manifest(HERE / "manifest.toml")


# --- the manifest -----------------------------------------------------------


def test_every_cell_expands_to_a_complete_argv(manifest):
    subs = {"input": "IN", "files_from": "LIST", "inputs_dir": "DIR", "out": "OUT", "setup": "SETUP"}
    for name, cell in manifest["cells"].items():
        argv = rm.build_argv(cell, subs)
        assert argv[0] == cell["command"], name
        assert "OUT" in " ".join(argv), f"{name} writes outside its scratch dir"
        if "setup" in cell:
            rm.build_argv(cell["setup"], subs)


def test_every_cell_names_its_zoom_or_reads_an_overview(manifest):
    for name, cell in manifest["cells"].items():
        assert ("zoom" in cell or "--max-zoom" in cell["flags"]
                or cell["command"] == "export-pmtiles"), name


def test_light_cells_run_at_least_three_pairs(manifest):
    for name, cell in manifest["cells"].items():
        if not cell["heavy"]:
            assert cell["pairs"] >= rm.MIN_PAIRS, name


def test_the_final_window_covers_every_light_cell(manifest):
    light = {n for n, c in manifest["cells"].items() if not c["heavy"]}
    sets = manifest["sets"]
    final = (set(sets["window1"]) | set(sets["final-gate"]) | set(sets["final-held"])
             | set(sets["release"]))
    assert final >= light
    assert not set(sets["window1"]) & set(sets["final-gate"])


def test_brazil_55m_is_the_whole_country(manifest):
    cell = manifest["cells"]["brazil-55m"]
    br = {n for n in manifest["inputs"] if n.startswith("ftw-br-")}
    assert set(cell["inputs"]) == br
    assert len(br) == 27  # noqa: PLR2004


def _write(tmp_path: Path, text: str) -> Path:
    p = tmp_path / "m.toml"
    p.write_text(text)
    return p


SHA = "0" * 64


def test_unknown_input_is_rejected(tmp_path):
    p = _write(tmp_path, f"""
[inputs.a]
file = "a.parquet"
sha256 = "{SHA}"
[cells.c]
input = "b"
command = "tiles"
args = ["{{input}}"]
output = "o"
""")
    with pytest.raises(rm.ManifestError, match="unknown input b"):
        rm.load_manifest(p)


def test_unknown_cell_key_is_rejected(tmp_path):
    p = _write(tmp_path, f"""
[inputs.a]
file = "a.parquet"
sha256 = "{SHA}"
[cells.c]
input = "a"
command = "tiles"
args = []
output = "o"
zooom = [0, 1]
""")
    with pytest.raises(rm.ManifestError, match="unknown keys"):
        rm.load_manifest(p)


def test_unfilled_placeholder_is_an_error():
    with pytest.raises(rm.ManifestError, match="unfilled"):
        rm.expand(["{nope}/x"], {"out": "o"})


# --- the run ------------------------------------------------------------------


def test_rounds_alternate_order():
    labels = ["base", "head"]
    assert [rm.round_order(labels, r) for r in range(4)] == [
        ["base", "head"], ["head", "base"], ["base", "head"], ["head", "base"]]


def test_zoom_flags():
    assert rm.zoom_flags({"zoom": [0, 12]}) == ["--min-zoom", "0", "--max-zoom", "12"]
    assert rm.zoom_flags({}) == []


def test_parse_gnu_time():
    stderr = (
        "\tUser time (seconds): 10.50\n\tSystem time (seconds): 1.25\n"
        "\tElapsed (wall clock) time (h:mm:ss or m:ss): 1:02.50\n"
        "\tMaximum resident set size (kbytes): 2048\n\tExit status: 0\n")
    assert rm.parse_gnu_time(stderr) == {
        "wall_s": 62.5, "cpu_s": 11.75, "peak_rss_kib": 2048, "exit_status": 0}


def test_detect_allocator(tmp_path):
    plain = tmp_path / "plain"
    plain.write_bytes(b"\x7fELF malloc free")
    mi = tmp_path / "mi"
    mi.write_bytes(b"\x7fELF mimalloc: option mi_option_verbose")
    assert rm.detect_allocator(plain) == "glibc"
    assert rm.detect_allocator(mi) == "mimalloc"
    stripped = tmp_path / "stripped"
    stripped.write_bytes(b'\x7fELF environment option "mimalloc_%s" is deprecated')
    assert rm.detect_allocator(stripped) == "mimalloc"
    docs = tmp_path / "docs"
    docs.write_bytes(b"\x7fELF --help: mimalloc is the default allocator")
    assert rm.detect_allocator(docs) == "glibc"


def test_sidecar_stands_in_for_rehash(tmp_path):
    f = tmp_path / "x.parquet"
    f.write_bytes(b"abc")
    want = rm.sha256_file(f)
    assert rm.verified(f, want)
    side = tmp_path / "x.parquet.sha256"
    assert side.read_text().split()[0] == want
    assert not rm.verified(f, "f" * 64)


def test_spread_and_summary():
    s = rm.spread([10.0, 12.0, 11.0])
    assert s == {"n": 3, "median": 11.0, "min": 10.0, "max": 12.0, "spread": 0.1818}

    def run(binary, wall, status="ok"):
        return {"binary": binary, "status": status, "wall_s": wall, "cpu_s": 1.0,
                "peak_rss_kib": 1024, "foreign_cores": 0.1,
                "output": {"sha256": "aa", "bytes": 5},
                "profile": {"export": {"phase_walls": {"total": wall / 2}}}}

    bins = [{"label": "a", "allocator": "glibc"}, {"label": "b", "allocator": "mimalloc"}]
    summ = rm.summarize([run("a", 2.0), run("a", 4.0), run("b", 1.0), run("b", 9.0, "OOM")], bins)
    assert summ["a"]["wall_s"]["median"] == 3.0  # noqa: PLR2004
    assert summ["a"]["export_s"]["median"] == 1.5  # noqa: PLR2004
    assert summ["b"]["wall_s"]["n"] == 1
    assert summ["b"]["statuses"] == ["OOM", "ok"]


def test_report_resumes_from_run_files(tmp_path):
    bins = [{"label": "a", "allocator": "glibc", "sha256": "1" * 64}]
    (tmp_path / "binaries.json").write_text(json.dumps(bins))
    cdir = tmp_path / "cell"
    cdir.mkdir()
    rec = {"binary": "a", "status": "ok", "wall_s": 2.0, "cpu_s": 1.0, "peak_rss_kib": 2048,
           "foreign_cores": 0.0, "output": {"sha256": "aa", "bytes": 5}, "profile": {}}
    (cdir / "a-r0.json").write_text(json.dumps(rec))
    rm.write_report(tmp_path)
    report = (tmp_path / "report.md").read_text()
    assert "| cell | a | 1 | ok | 2.00 s |" in report


def test_parse_cpus():
    assert rm.parse_cpus("0-3") == {0, 1, 2, 3}
    assert rm.parse_cpus("0,2,4-5") == {0, 2, 4, 5}


def test_busy_jiffies_of_a_cpu_set_add_up():
    cpus = rm.all_cpus()
    half = frozenset(sorted(cpus)[: len(cpus) // 2])
    whole_busy, whole_total = rm.cpu_busy_jiffies(cpus)
    part_busy, part_total = rm.cpu_busy_jiffies(half)
    assert 0 < part_total < whole_total
    assert part_busy <= whole_busy


def test_spill_codec_cell_watches_its_spill_dir(manifest):
    cell = manifest["cells"]["spill-codec"]
    argv = rm.build_argv(cell, {"input": "IN", "out": "OUT"})
    assert argv[argv.index("--spill-dir") + 1] == f"OUT/{cell['watch']}"
    assert cell["env"]["TYLERTOO_AUTO_MEM_LIMIT_BYTES"] == "1048576"


def test_warmup_records_stay_out_of_the_summary(tmp_path):
    cdir = tmp_path / "cell"
    (cdir / "warmup").mkdir(parents=True)
    rec = {"binary": "a", "status": "ok", "wall_s": 9.0}
    (cdir / "warmup" / "a-w0.json").write_text(json.dumps(rec))
    (cdir / "a-r0.json").write_text(json.dumps({**rec, "wall_s": 1.0}))
    assert [r["wall_s"] for r in rm.load_runs(cdir)] == [1.0]


def test_report_splits_three_binaries_against_first_and_previous(tmp_path):
    bins = [{"label": n, "allocator": "glibc", "sha256": str(i) * 64}
            for i, n in enumerate(["main", "head-glibc", "head"])]
    (tmp_path / "binaries.json").write_text(json.dumps(bins))
    cdir = tmp_path / "cell"
    cdir.mkdir()
    for name, wall in [("main", 10.0), ("head-glibc", 8.0), ("head", 6.0)]:
        rec = {"binary": name, "status": "ok", "wall_s": wall, "cpu_s": 1.0, "peak_rss_kib": 1024,
               "foreign_cores": 0.0, "output": {"sha256": "aa", "bytes": 5}, "profile": {}}
        (cdir / f"{name}-r0.json").write_text(json.dumps(rec))
    rm.write_report(tmp_path)
    row = next(ln for ln in (tmp_path / "report.md").read_text().splitlines() if "| head |" in ln)
    assert "| 0.600 | 0.750 |" in row


def test_device_of_the_worktree_has_a_write_counter():
    dev = rm.block_device(HERE)
    assert rm.sectors_written(dev) >= 0


def test_report_lists_pass2_writer_finish_spill_and_disk(tmp_path):
    bins = [{"label": n, "allocator": "glibc", "sha256": "0" * 64} for n in ("raw", "lz4")]
    (tmp_path / "binaries.json").write_text(json.dumps(bins))
    cdir = tmp_path / "spill-codec"
    cdir.mkdir()
    for name, p2 in [("raw", 9.0), ("lz4", 7.0)]:
        rec = {"binary": name, "status": "ok", "wall_s": p2 + 1, "cpu_s": 1.0, "peak_rss_kib": 1024,
               "foreign_cores": 0.0, "output": {"sha256": "aa", "bytes": 5},
               "watch_peak_mib": 100.0, "device_write_mib": 50.0, "gate": {"disk_write_mbps": 3.5},
               "profile": {"convert": {"phase_walls": {"pass2": p2, "writer_finish": 0.5, "total": p2 + 1}}}}
        (cdir / f"{name}-r0.json").write_text(json.dumps(rec))
    rm.write_report(tmp_path)
    row = [ln for ln in (tmp_path / "report.md").read_text().splitlines() if ln.startswith("| spill-codec | lz4")][-1]
    assert row == "| spill-codec | lz4 | 7.00 | 0.50 | 100.00 | 3.50 | 50.00 |"


def _bins(sha: str = "1" * 64, env: dict | None = None) -> list[dict]:
    return [{"label": "a", "path": "/x/a", "allocator": "glibc", "sha256": sha,
             "env": env or {}, "affinity_cpus": 8}]


def test_resume_with_the_same_binaries_is_allowed(tmp_path):
    saved = tmp_path / "binaries.json"
    rm.check_resume(saved, _bins())  # nothing saved yet
    saved.write_text(json.dumps(_bins()))
    moved = [{**_bins()[0], "path": "/y/a"}]  # same build, another path
    rm.check_resume(saved, moved)


@pytest.mark.parametrize("given", [_bins(sha="2" * 64), _bins(env={"K": "V"}),
                                   [{**_bins()[0], "allocator": "mimalloc"}]])
def test_resume_with_other_binaries_is_refused(tmp_path, given):
    saved = tmp_path / "binaries.json"
    saved.write_text(json.dumps(_bins()))
    with pytest.raises(rm.ManifestError, match="new --name"):
        rm.check_resume(saved, given)


def test_a_run_whose_gate_timed_out_stays_out_of_the_medians():
    bins = [{"label": "a", "allocator": "glibc"}]

    def run(wall: float, gate: str) -> dict:
        return {"binary": "a", "status": "ok", "wall_s": wall, "cpu_s": 1.0, "peak_rss_kib": 1024,
                "foreign_cores": 0.0, "gate": {"gate": gate}, "profile": {},
                "output": {"sha256": "aa", "bytes": 5}}

    summ = rm.summarize([run(1.0, "quiet"), run(3.0, "quiet"), run(50.0, "TIMED_OUT")], bins)
    assert summ["a"]["wall_s"]["n"] == 2  # noqa: PLR2004
    assert summ["a"]["wall_s"]["median"] == 2.0  # noqa: PLR2004
    assert summ["a"]["statuses"] == ["GATE_TIMED_OUT", "ok"]


# --- pinned inputs (#810) -----------------------------------------------------

CORPUS = HERE.parent / "corpus" / "manifest.json"


def test_one_overture_release_across_corpus_and_manifest(manifest):
    corpus = json.loads(CORPUS.read_text())
    release = corpus["overture"]["release"]
    for name, inp in manifest["inputs"].items():
        if "overture" in inp.get("url", "") + inp.get("file", ""):
            assert f"/{release}/" in inp.get("url", "") and release in inp["file"], name


def test_every_corpus_dataset_is_pinned():
    corpus = json.loads(CORPUS.read_text())
    for ds in corpus["datasets"]:
        key = "ids_sha256" if ds["source"] == "overture" else "sha256"
        assert len(ds.get(key, "")) == 64, f"{ds['id']}: no {key}"  # noqa: PLR2004
        assert ds["verified_count"] is True and isinstance(ds["expected_features"], int), ds["id"]


# --- CPU, saturation and provenance (#810) ---------------------------------

def _ok(binary: str, wall: float, cpu: float, profile: dict | None = None) -> dict:
    return {"binary": binary, "status": "ok", "wall_s": wall, "cpu_s": cpu, "peak_rss_kib": 1024,
            "foreign_cores": 0.0, "output": {"sha256": "aa", "bytes": 5}, "profile": profile or {}}


def test_summary_keeps_whole_run_effective_cores():
    bins = [{"label": "a", "allocator": "glibc"}]
    summ = rm.summarize([_ok("a", 2.0, 8.0), _ok("a", 4.0, 8.0), _ok("a", 1.0, 3.0)], bins)
    assert summ["a"]["effective_cores"]["median"] == 3.0  # noqa: PLR2004
    assert rm.effective_cores({"wall_s": 2.0, "cpu_s": None}) is None


PHASE = {"cpu_user_secs": 6.0, "cpu_sys_secs": 2.0, "effective_cores": 4.0, "utilization": 0.5,
         "unattributed_core_secs": 1.0, "read_bytes": 10, "write_bytes": 20}


def _profile_lines(tmp_path: Path, with_810: bool) -> Path:
    sampler = {"true_peak_mib": 10.0}
    if with_810:
        sampler |= {"phase_order": ["pass1 scan"], "phases": {"pass1 scan": {**PHASE, "extra": 1}},
                    "timeline": [[0.0, 0.0, 1, 0, 0, 0]] * 3}
    convert = {"phase": "convert", "phase_walls": {"total": 2.0}, "rss_sampler": sampler,
               "pass2": {"rows": 5, **({"writer_busy_secs": 0.7} if with_810 else {})}}
    export = {"phase": "export", "export": {
        "phase_walls": {"total": 1.0}, "rss_sampler": {"phases": {"levels": PHASE}} if with_810 else {},
        **({"serial_secs": {"spool_write": 0.1, "finalize": 0.2, "group_commit_wait": 0.0}}
           if with_810 else {})}}
    p = tmp_path / "p.jsonl"
    p.write_text(json.dumps(convert) + "\n" + json.dumps(export) + "\n")
    return p


def test_profile_keeps_the_per_phase_saturation_fields(tmp_path):
    prof = rm.read_profile(_profile_lines(tmp_path, with_810=True))
    assert prof["convert"]["phases"] == {"pass1 scan": PHASE}
    assert prof["convert"]["writer_busy_secs"] == 0.7  # noqa: PLR2004
    assert prof["export"]["phases"] == {"levels": PHASE}
    assert prof["export"]["serial_secs"]["finalize"] == 0.2  # noqa: PLR2004
    assert "timeline" not in json.dumps(prof)  # the JSONL keeps it; the record does not


def test_profile_without_the_saturation_fields_still_reads(tmp_path):
    prof = rm.read_profile(_profile_lines(tmp_path, with_810=False))
    assert prof["convert"]["phases"] == {}
    assert prof["convert"]["writer_busy_secs"] is None
    assert prof["export"]["serial_secs"] is None


def test_summary_carries_per_phase_effective_cores(tmp_path):
    prof = rm.read_profile(_profile_lines(tmp_path, with_810=True))
    bins = [{"label": "a", "allocator": "glibc"}]
    summ = rm.summarize([_ok("a", 2.0, 4.0, prof), _ok("a", 2.0, 4.0, prof)], bins)
    assert summ["a"]["phase_effective_cores"] == {"convert/pass1 scan": rm.spread([4.0, 4.0]),
                                                  "export/levels": rm.spread([4.0, 4.0])}


# The keys scripts/gen_bench_docs.py requires of every benchmarks/results file.
_spec = importlib.util.spec_from_file_location("gen_bench_docs", HERE.parent / "scripts/gen_bench_docs.py")
_docs = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_docs)
PROVENANCE_KEYS = _docs.PROVENANCE_KEYS + ["git_describe", "host_class", "release_version"]


def test_provenance_has_every_contract_key(tmp_path, monkeypatch):
    original_out = rm._out
    monkeypatch.setattr(rm, "_out", lambda argv: "checkout" if argv[0] == "git" else original_out(argv))
    monkeypatch.setattr(rm, "release_version", lambda root: "9.9.9")
    exe = tmp_path / "tylertoo"
    exe.write_text("#!/bin/sh\necho tylertoo 9.9.9\n")
    exe.chmod(0o755)
    bins = [{"label": "head", "path": str(exe), "allocator": "mimalloc", "sha256": "1" * 64}]
    inputs = {"x": {"file": "x.parquet", "sha256": "2" * 64,
                    "url": "s3://overturemaps-us-west-2/release/2026-10-21.0/theme=a/b.parquet"}}
    p = rm.provenance(bins, inputs)
    assert list(p) == PROVENANCE_KEYS
    assert p["tylertoo_version"] == "tylertoo 9.9.9"
    assert p["allocator"] == "mimalloc"
    assert p["tool_versions"]["head"] == "tylertoo 9.9.9"
    assert p["inputs"] == [{"name": "x", "sha256": "2" * 64, "release": "2026-10-21.0",
                            "url": inputs["x"]["url"]}]
    assert p["nproc"] >= 1
    assert p["git_describe"] == "checkout"
    assert p["host_class"] == rm.host_class_key()
    assert p["release_version"] == "9.9.9"


def test_report_json_out_writes_a_results_file(tmp_path):
    bins = [{"label": "a", "allocator": "glibc", "sha256": "1" * 64}]
    (tmp_path / "binaries.json").write_text(json.dumps(bins))
    (tmp_path / "provenance.json").write_text(json.dumps({k: None for k in PROVENANCE_KEYS}))
    cdir = tmp_path / "cell"
    cdir.mkdir()
    (cdir / "a-r0.json").write_text(json.dumps(_ok("a", 2.0, 6.0)))
    out = tmp_path / "results" / "x.json"
    rm.write_results_json(tmp_path, out)
    doc = json.loads(out.read_text())
    assert list(doc["provenance"]) == PROVENANCE_KEYS
    assert doc["cells"]["cell"]["a"]["effective_cores"]["median"] == 3.0  # noqa: PLR2004


@pytest.mark.skipif(not Path("/usr/bin/time").exists(), reason="needs GNU time")
def test_timed_records_cpu_and_effective_cores(tmp_path):
    res = rm.timed(["sh", "-c", "i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done"],
                   tmp_path / "log.txt")
    assert res["status"] == "ok"
    assert res["cpu_s"] is not None and res["effective_cores"] is not None
    assert res["profile"] == {}  # no TYLERTOO_PROFILE_JSON written by sh


def test_profile_keeps_per_zoom_wave_width_and_spill(tmp_path):
    export = {"phase": "export", "export": {"per_zoom": [
        {"zoom": 0, "tiles": 1, "wave_width": 4, "spill_bytes": 10},
        {"zoom": 1, "tiles": 4}]}}
    p = tmp_path / "p.jsonl"
    p.write_text(json.dumps(export) + "\n")
    ex = rm.read_profile(p)["export"]
    assert ex["levels"] == [{"zoom": 0, "wave_width": 4, "spill_bytes": 10},
                            {"zoom": 1, "wave_width": None, "spill_bytes": None}]
    assert ex["tiles"] == 5  # noqa: PLR2004


def test_keep_output_copies_the_archive_out_of_scratch(tmp_path):
    out = tmp_path / "out"
    out.mkdir()
    (out / "o.pmtiles").write_bytes(b"PM")
    kept = rm.keep_output(out, "o.pmtiles", tmp_path / "cell", "head-r0")
    assert kept.read_bytes() == b"PM" and kept.parent == tmp_path / "cell"
    assert rm.keep_output(out, "missing", tmp_path / "cell", "x") is None


def test_keep_output_is_a_cell_key(tmp_path):
    p = _write(tmp_path, f"""
[inputs.a]
file = "a.parquet"
sha256 = "{SHA}"
[cells.c]
input = "a"
command = "tiles"
args = []
output = "o"
keep_output = true
""")
    assert rm.load_manifest(p)["cells"]["c"]["keep_output"] is True


def test_a_resume_keeps_the_recorded_provenance(tmp_path, monkeypatch):
    """Sol review of #810: a resume rewrote provenance.json from the current
    checkout and date, relabelling runs it did not repeat."""
    from types import SimpleNamespace
    (tmp_path / "provenance.json").write_text(json.dumps(
        {"commit": "A", "date": "then", "inputs": [{"name": "x"}]}))
    monkeypatch.setattr(rm, "provenance", lambda bins, inputs: {
        "commit": "B", "date": "now", "inputs": [{"name": n} for n in inputs]})
    monkeypatch.setattr(rm, "write_report", lambda results: None)
    fake = SimpleNamespace(results=tmp_path, bins=[], m={"inputs": {"x": {}, "y": {}},
                           "cells": {"c": {"input": "x"}, "d": {"input": "y"}}},
                           cells=lambda: ["c", "d"], run_cell=lambda name: None)
    rm.Runner.run_all(fake)
    saved = json.loads((tmp_path / "provenance.json").read_text())
    assert (saved["commit"], saved["date"]) == ("A", "then")
    assert [i["name"] for i in saved["inputs"]] == ["x", "y"]


def _export_line(tmp_path: Path, share) -> Path:
    ex = {"phase_walls": {"total": 1.0}, **({} if share is None else {"serial_share": share})}
    p = tmp_path / "s.jsonl"
    p.write_text(json.dumps({"phase": "export", "export": ex}) + "\n")
    return p


def test_profile_keeps_serial_share(tmp_path):
    assert rm.read_profile(_export_line(tmp_path, 0.07))["export"]["serial_share"] == 0.07  # noqa: PLR2004
    assert rm.read_profile(_export_line(tmp_path, None))["export"]["serial_share"] is None


def test_summary_carries_a_serial_share_spread(tmp_path):
    bins = [{"label": "a", "allocator": "glibc"}]
    runs = [_ok("a", 2.0, 4.0, rm.read_profile(_export_line(tmp_path, s))) for s in (0.05, 0.09)]
    assert rm.summarize(runs, bins)["a"]["serial_share"] == rm.spread([0.05, 0.09])


# --- nightly and release suite -----------------------------------------------

@pytest.mark.parametrize("tier", ["nightly", "release"])
def test_suite_sets_resolve_without_brazil(manifest, tier):
    members = manifest["sets"][tier]
    assert members
    assert set(members) <= manifest["cells"].keys()
    for name in members:
        for inp in rm.cell_input_names(manifest["cells"][name]):
            assert not inp.startswith("ftw-br-") and "brazil" not in inp.lower()


def test_nightly_inputs_are_fetchable_and_pinned(manifest):
    for name in manifest["sets"]["nightly"]:
        for inp in rm.cell_input_names(manifest["cells"][name]):
            assert manifest["inputs"][inp]["url"]
            assert len(manifest["inputs"][inp]["sha256"]) == 64


INPUT_RELEASE = "https://github.com/geoparquet-io/tylertoo/releases/download/bench-inputs-v2"


@pytest.mark.parametrize("suffix,version,size,sha", [
    ("geoarrow", "1.1-geoarrow", 78782752, "5774a5c66b8600a2ef728aaf8cea5aa995b9129b24648039aef8ab361a8c4c5e"),
    ("geo2", "2.0", 98501420, "81ed422107b17622a98e5134077e2f9dc06d0de36c03d45f229005302913e941"),
    ("geo-only", "parquet-geo-only", 98500905, "58636fcba98cb6a802334d15a6a10e4a36e92becb3113a15cc3b4eff39759311"),
])
def test_encoding_variants_are_pinned(manifest, suffix, version, size, sha):
    inp = manifest["inputs"][f"moldova-{suffix}"]
    assert inp["file"] == f"moldova-hilbert-{suffix}.parquet"
    assert inp["url"] == f"{INPUT_RELEASE}/{inp['file']}"
    assert (inp["bytes"], inp["sha256"], inp["rows"], inp["geometry"]) == (size, sha, 631910, "Polygon")
    assert "1.7.0" in inp["notes"]
    assert f"gpio convert geoparquet moldova-hilbert.parquet {inp['file']} --geoparquet-version {version}" in inp["notes"]
    cell = manifest["cells"][f"in-{suffix}"]
    assert (cell["input"], cell["command"], cell["zoom"], cell["flags"]) == (f"moldova-{suffix}", "tiles", [0, 12], [])


def test_multipart_sources_and_bounded_twins(manifest):
    parts = [f"moldova-part-{i:03}" for i in range(13)]
    for i, name in enumerate(parts):
        inp = manifest["inputs"][name]
        assert inp["file"] == f"part-{i:03}.parquet"
        assert inp["url"] == f"{INPUT_RELEASE}/{name}.parquet"
    for name in ("in-parts13", "in-dir13", "in-http13"):
        cell = manifest["cells"][name]
        assert cell["inputs"] == parts
        assert (cell["command"], cell["zoom"]) == ("tiles", [0, 12])
        if name == "in-dir13":
            assert cell["args"] == ["{inputs_dir}", "{out}/out.pmtiles"]
        else:
            assert "--files-from" in cell["args"] + cell["flags"]
    assert manifest["cells"]["in-http13"]["serve"] == {"ttfb_ms": 100}
    for name in ("tiles-madagascar", "polygons", "tiles-nz"):
        base, bounded = manifest["cells"][name], manifest["cells"][f"{name}-bounded"]
        assert bounded["flags"] == ["--profile", "bounded"]
        for key in ("input", "command", "args", "zoom", "wall_s", "rss_gib"):
            assert bounded[key] == base[key]


def test_fetch_uses_manifest_filename_and_checks_hash(tmp_path, monkeypatch):
    monkeypatch.setattr(rm.urllib.request, "urlopen", lambda req: io.BytesIO(b"parquet"))
    digest = rm.hashlib.sha256(b"parquet").hexdigest()
    inp = {"file": "part-000.parquet", "url": f"{INPUT_RELEASE}/moldova-part-000.parquet", "sha256": digest}
    rm.fetch({"inputs": {"part": inp}}, tmp_path, [], ["part"])
    assert (tmp_path / "inputs/part-000.parquet").read_bytes() == b"parquet"
    assert not (tmp_path / "inputs/moldova-part-000.parquet").exists()
    inp["sha256"] = "0" * 64
    with pytest.raises(rm.ManifestError, match="sha256"):
        rm.fetch({"inputs": {"part": inp}}, tmp_path, [], ["part"])


def test_inputs_dir_substitution_symlinks_every_manifest_file(tmp_path, manifest):
    name = "in-dir13"
    cell = manifest["cells"][name]
    (tmp_path / "results" / name).mkdir(parents=True)
    local = []
    for i, inp in enumerate(cell["inputs"]):
        path = tmp_path / f"cached-{i}.parquet"
        path.write_bytes(inp.encode())
        local.append(path)
    runner = SimpleNamespace(results=tmp_path / "results", bench_out=tmp_path / "scratch",
                             args=SimpleNamespace(name="suite"), m=manifest)
    subs = rm.Runner.substitutions(runner, name, local, None)
    directory = Path(subs["inputs_dir"])
    assert directory.is_relative_to(runner.bench_out / runner.args.name)
    assert {p.name for p in directory.iterdir()} == {manifest["inputs"][n]["file"] for n in cell["inputs"]}
    for inp, path in zip(cell["inputs"], local, strict=True):
        link = directory / manifest["inputs"][inp]["file"]
        assert link.is_symlink() and link.resolve() == path
    assert str(directory) in rm.build_argv(cell, {**subs, "out": "OUT"})


def _gate_run(tmp_path, head_wall=11.0, head_peak=110.0, labels=("base", "head")):
    results = tmp_path / "results" / "suite"
    results.mkdir(parents=True)
    bins = [{"label": label, "allocator": "glibc"} for label in labels]
    (results / "binaries.json").write_text(json.dumps(bins))
    # Outliers and a warm-up must not change the measured medians.
    for name, wall in (("slow", head_wall), ("fast", 9.0)):
        cell = results / name
        cell.mkdir()
        for label, wall_s, peak in ((labels[0], 10.0, 100.0), (labels[1], wall, head_peak)):
            for i, factor in enumerate((0.5, 1, 10)):
                rec = {**_ok(label, wall_s * factor, 1.0), "peak_rss_kib": peak * factor * 1024}
                (cell / f"{label}-r{i}.json").write_text(json.dumps(rec))
        (cell / "warmup").mkdir()
        (cell / "warmup/head-w0.json").write_text(json.dumps(_ok(labels[1], 999, 1)))
    baseline = tmp_path / "baseline.json"
    baseline.write_text(json.dumps({"hosts": {}}))
    return ["gate", "--data", str(tmp_path), "--name", "suite", "--wall-max", "1.25",
            "--rss-max", "1.15", "--baseline", str(baseline)]


@pytest.mark.parametrize("wall,peak,status", [(11, 110, 0), (12.5, 115, 0), (12.51, 100, 1), (10, 115.1, 1)])
def test_gate_head_base_limits_and_ranked_table(tmp_path, capsys, wall, peak, status):
    assert rm.main(_gate_run(tmp_path, wall, peak)) == status
    text = capsys.readouterr().out
    assert "| cell | base wall" in text
    assert text.index("| slow |") < text.index("| fast |")
    assert ("FAIL" in text) == bool(status)


@pytest.mark.parametrize("host,wall,peak,status", [
    ("other", 1, 1, 0), ("matching", 10, 100, 0),
    ("matching", 7, 100, 1), ("matching", 10, 90, 1),
])
def test_gate_baseline_requires_matching_host(tmp_path, monkeypatch, host, wall, peak, status):
    argv = _gate_run(tmp_path)
    monkeypatch.setattr(rm, "host_class_key", lambda: "matching")
    baseline = {"hosts": {host: {"slow": {"wall_s": wall, "peak_rss_bytes": peak * (1 << 20)}}}}
    (tmp_path / "baseline.json").write_text(json.dumps(baseline))
    assert rm.main(argv) == status


def test_gate_records_head_medians_and_preserves_other_hosts(tmp_path, monkeypatch):
    argv = _gate_run(tmp_path, labels=("old", "new"))
    monkeypatch.setattr(rm, "host_class_key", lambda: "matching")
    original = {"hosts": {"other": {"saved": {"wall_s": 1, "peak_rss_bytes": 2}}}}
    source = tmp_path / "baseline.json"
    source.write_text(json.dumps(original))
    out = tmp_path / "recorded.json"
    assert rm.main(argv + ["--base", "old", "--head", "new", "--record-baseline", str(out)]) == 0
    doc = json.loads(out.read_text())
    assert doc["hosts"]["matching"]["slow"] == {"wall_s": 11, "peak_rss_bytes": 110 * (1 << 20)}
    assert doc["hosts"]["other"] == original["hosts"]["other"]
    assert json.loads(source.read_text()) == original


@pytest.mark.parametrize("bad", ["empty-cell", "missing-head", "OOM", "TIMED_OUT"])
def test_gate_rejects_missing_or_failed_measurements(tmp_path, bad):
    argv = _gate_run(tmp_path)
    cell = tmp_path / "results/suite/slow"
    for path in cell.glob("*-r*.json" if bad == "empty-cell" else "head-r*.json"):
        if bad in ("empty-cell", "missing-head"):
            path.unlink()
        else:
            rec = json.loads(path.read_text())
            if bad == "OOM":
                rec["status"] = bad
            else:
                rec["gate"] = {"gate": bad}
            path.write_text(json.dumps(rec))
    assert rm.main(argv) == 1


@pytest.mark.parametrize("tag,head,status,expected", [
    ("abc", "abc", "", "9.9.9"), (None, "abc", "", "9.9.9"),
    (None, "abc", " M docs/page.md\0?? benchmarks/results/new.json\0 M README.md\0 M crates/core/README.md\0", "9.9.9"),
    (None, "abc", " M src/main.rs\0", None), ("abc", "abc", "?? scratch\0", None),
    ("abc", "def", "", None), (None, "abc", "?? crates/core/src/README.md\0", None),
    (None, "abc", "R  docs/renamed.md\0src/old.rs\0", None),
])
def test_release_version_checks_tag_and_rc_tree(tmp_path, monkeypatch, tag, head, status, expected):
    (tmp_path / "Cargo.toml").write_text('[workspace.package]\nversion = "9.9.9"\n')
    def git(argv, **kwargs):
        assert argv[:3] == ["git", "-C", str(tmp_path)]
        if "status" in argv:
            return SimpleNamespace(returncode=0, stdout=status)
        if "HEAD" in argv:
            return SimpleNamespace(returncode=0, stdout=head + "\n")
        if "tag" in argv:
            return SimpleNamespace(returncode=0, stdout="v9.9.9\n" if tag else "")
        assert "refs/tags/v9.9.9^{commit}" in argv
        return SimpleNamespace(returncode=0 if tag else 1, stdout=(tag or "") + "\n")
    monkeypatch.setattr(rm.subprocess, "run", git)
    assert rm.release_version(tmp_path) == expected


def test_release_version_does_not_treat_git_errors_as_missing_tags(tmp_path, monkeypatch):
    (tmp_path / "Cargo.toml").write_text('[workspace.package]\nversion = "9.9.9"\n')
    def git(argv, **kwargs):
        if "HEAD" in argv:
            return SimpleNamespace(returncode=0, stdout="abc\n")
        if "status" in argv:
            return SimpleNamespace(returncode=0, stdout="")
        raise OSError("cannot inspect tags")
    monkeypatch.setattr(rm.subprocess, "run", git)
    assert rm.release_version(tmp_path) is None


@pytest.mark.parametrize("version", ["9.9.9", None])
def test_release_version_subcommand_prints_only_version(monkeypatch, capsys, version):
    monkeypatch.setattr(rm, "release_version", lambda root: version)
    assert rm.main(["release-version"]) == 0
    assert capsys.readouterr().out == (version + "\n" if version else "")


@pytest.mark.parametrize("saved", [None, {"commit": "original", "date": "then", "inputs": []}])
def test_report_json_completes_legacy_provenance_without_relabelling(tmp_path, monkeypatch, saved):
    # A report must not attribute old measurements to the current checkout.
    monkeypatch.setattr(rm, "provenance", lambda *args: pytest.fail("report recollected provenance"))
    bins = [{"label": "head", "allocator": "glibc", "sha256": "1" * 64}]
    (tmp_path / "binaries.json").write_text(json.dumps(bins))
    if saved is not None:
        (tmp_path / "provenance.json").write_text(json.dumps(saved))
    out = tmp_path / "report.json"
    assert rm.main(["report", "--name", "suite", "--results", str(tmp_path), "--json-out", str(out)]) == 0
    prov = json.loads(out.read_text())["provenance"]
    assert set(PROVENANCE_KEYS) <= prov.keys()
    assert all(prov[k] == v for k, v in (saved or {}).items())
    assert all(prov[k] is None for k in PROVENANCE_KEYS if k not in (saved or {}))


@pytest.mark.parametrize("metric,limit", [("wall_s", "BASELINE_WALL_MAX"), ("peak_rss_bytes", "BASELINE_RSS_MAX")])
@pytest.mark.parametrize("factor,status", [(1, 0), (1.001, 1)])
def test_gate_baseline_thresholds_come_from_constants(tmp_path, monkeypatch, metric, limit, factor, status):
    argv = _gate_run(tmp_path, head_wall=14, head_peak=120)
    argv[argv.index("--wall-max") + 1] = "2"
    argv[argv.index("--rss-max") + 1] = "2"
    monkeypatch.setattr(rm, "host_class_key", lambda: "matching")
    # These medians are exactly at the baseline thresholds before factor is applied.
    entry = {"wall_s": 14 / rm.BASELINE_WALL_MAX, "peak_rss_bytes": 120 * (1 << 20) / rm.BASELINE_RSS_MAX}
    head = {"wall_s": 14, "peak_rss_bytes": 120 * (1 << 20)}
    entry[metric] = head[metric] / getattr(rm, limit) / factor
    (tmp_path / "baseline.json").write_text(json.dumps({"hosts": {"matching": {"slow": entry}}}))
    assert rm.main(argv) == status


def test_failed_round_is_retried_without_repeating_successes(tmp_path, monkeypatch):
    """Release resumes must not treat an OOM or expired gate as completed."""
    cdir = tmp_path / 'results/c'
    cdir.mkdir(parents=True)
    status = {'status': 'KILLED', 'returncode': -9, 'wall_s': 1, 'cpu_s': 1,
              'peak_rss_kib': 1, 'scratch_peak_mib': 0, 'foreign_cores': 0}
    gate = SimpleNamespace(wait=lambda: {'gate': 'quiet'}, last_own_end=0)
    runner = SimpleNamespace(results=tmp_path / 'results', bench_out=tmp_path / 'scratch',
        args=SimpleNamespace(name='release', dry_run=False), bins=[{'label': 'head', 'path': 'fake',
        'allocator': 'any', 'env': {'EXPLICIT': 'yes'}, 'sha256': 'abc'}],
        min_free_gb=0, gate=gate, disk=None, cpus=None)
    cell = {'env': {'AWS_SKIP_SIGNATURE': 'true', 'COUNT': 4}, 'output': 'out.pmtiles', 'allocator': 'any',
            'command': 'tiles', 'args': ['{out}/out.pmtiles'], 'flags': []}
    monkeypatch.setattr(rm, 'require_disk', lambda *a: None)
    monkeypatch.setattr(rm, 'sectors_written', lambda *a: 0)
    monkeypatch.setattr(rm, 'read_profile', lambda *a: {})
    monkeypatch.setattr(rm, 'output_digest', lambda *a: {'bytes': 1, 'sha256': 'x'})
    monkeypatch.setattr(rm.subprocess, 'run', lambda *a, **k: None)  # sync only
    calls = []
    def measured(*args):
        calls.append(args)
        return dict(status)
    monkeypatch.setattr(rm, 'run_capped', measured)
    with pytest.raises(rm.ManifestError, match='failed'):
        rm.Runner.run_one(runner, 'c', cell, 0, 'head', {}, [], None)
    saved = json.loads((cdir / 'head-r0.json').read_text())
    assert saved['env'] == {'AWS_SKIP_SIGNATURE': 'true', 'COUNT': '4', 'EXPLICIT': 'yes'}
    status.update(status='ok', returncode=0)
    rm.Runner.run_one(runner, 'c', cell, 0, 'head', {}, [], None)
    rm.Runner.run_one(runner, 'c', cell, 0, 'head', {}, [], None)
    assert len(calls) == 2
    assert rm.successful_run(cdir / 'head-r0.json')
    assert not rm.successful_run(cdir / 'missing.json')


def test_failed_summary_does_not_skip_cell(tmp_path, monkeypatch):
    cdir = tmp_path / 'c'
    cdir.mkdir()
    (cdir / 'summary.json').write_text('{}')
    (cdir / 'head-r0.json').write_text(json.dumps({'status': 'KILLED'}))
    # Input resolution is reached only when the failed summary was not skipped.
    def resolve(*args):
        raise RuntimeError('resuming failed cell')
    monkeypatch.setattr(rm, 'resolve_input', resolve)
    runner = SimpleNamespace(results=tmp_path, args=SimpleNamespace(force=False),
        m={'cells': {'c': {'input': 'a'}}, 'inputs': {'a': {}}}, bins=[{'label': 'head'}],
        pairs=lambda cell: 1, data=tmp_path, input_dirs=[])
    with pytest.raises(RuntimeError, match='resuming failed cell'):
        rm.Runner.run_cell(runner, 'c')
