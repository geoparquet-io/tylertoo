"""
Tests for run_manifest.py and the manifest it reads (#734).

    uv run --no-project --with pytest pytest benchmarks/test_run_manifest.py
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import run_manifest as rm  # noqa: E402


@pytest.fixture(scope="module")
def manifest() -> dict:
    return rm.load_manifest(HERE / "manifest.toml")


# --- the manifest -----------------------------------------------------------


def test_every_cell_expands_to_a_complete_argv(manifest):
    subs = {"input": "IN", "files_from": "LIST", "out": "OUT", "setup": "SETUP"}
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
    final = set(sets["window1"]) | set(sets["final-gate"]) | set(sets["final-held"])
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
