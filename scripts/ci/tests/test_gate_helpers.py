"""Tests for the gate-canary, staleness and mutation-gate helpers (#779)."""

from __future__ import annotations

import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "ci"))

import canary  # noqa: E402
import mutation_gate  # noqa: E402
import staleness  # noqa: E402

# --- canary -----------------------------------------------------------------


def write(tmp_path: Path, name: str, text: str) -> Path:
    p = tmp_path / name
    p.write_text(text)
    return p


PATCH_BODY = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n"


def test_header_parsing(tmp_path: Path) -> None:
    p = write(
        tmp_path,
        "g9-thing.patch",
        "# gate: cargo fmt --all -- --check\n# expect: Diff in\n# setup: tippecanoe\n# issue: #1\n" + PATCH_BODY,
    )
    c = canary.parse_header(p)
    assert (c.name, c.gate, c.expect, c.setup, c.issue) == (
        "g9-thing",
        "cargo fmt --all -- --check",
        "Diff in",
        ["tippecanoe"],
        "#1",
    )
    assert c.matrix_entry()["tippecanoe"] is True and c.matrix_entry()["go_pmtiles"] is False


def test_header_needs_a_gate(tmp_path: Path) -> None:
    with pytest.raises(canary.CanaryError, match="gate"):
        canary.parse_header(write(tmp_path, "a.patch", "# expect: x\n" + PATCH_BODY))


def test_header_rejects_unknown_setup_and_bad_regex(tmp_path: Path) -> None:
    with pytest.raises(canary.CanaryError, match="unknown"):
        canary.parse_header(write(tmp_path, "a.patch", "# gate: true\n# setup: docker\n" + PATCH_BODY))
    with pytest.raises(canary.CanaryError, match="regex"):
        canary.parse_header(write(tmp_path, "b.patch", "# gate: true\n# expect: (\n" + PATCH_BODY))


def test_header_stops_at_the_patch(tmp_path: Path) -> None:
    # A `# gate:` line inside the diff body is content, not a header.
    with pytest.raises(canary.CanaryError):
        canary.parse_header(write(tmp_path, "a.patch", PATCH_BODY + "# gate: true\n"))


@pytest.mark.parametrize(
    ("code", "output", "expect", "ok"),
    [
        (1, "Diff in src/lib.rs", "Diff in", True),
        (1, "anything", None, True),
        (0, "all good", None, False),  # the gate passed: vacuous
        (127, "command not found", None, False),  # rotted
        (101, "error: could not compile (linker)", "len_zero", False),  # red for the wrong reason
    ],
)
def test_verdict(code: int, output: str, expect: str | None, ok: bool) -> None:
    assert canary.verdict(code, output, expect)[0] is ok


def test_every_committed_canary_parses() -> None:
    found = canary.discover()
    assert found, "ci/canaries/ holds no canary"
    names = [c.name for c in found]
    assert len(names) == len(set(names))
    for c in found:
        assert c.path.read_text().count("diff --git") >= 1, f"{c.name} holds no patch"


def test_ci_step_names_real_steps() -> None:
    # Every canary that runs a workflow step must name one that exists and
    # is runnable, or the nightly reports it rotted.
    for c in canary.discover():
        if "canary.py ci-step" in c.gate:
            import shlex

            argv = shlex.split(c.gate)
            wf, job, step = argv[argv.index("ci-step") + 1 :][:3]
            assert "run" in canary.find_step(wf, job, step)


def test_ci_step_refuses_expressions() -> None:
    with pytest.raises(canary.CanaryError, match="expression"):
        canary.find_step("ci.yml", "coverage", "Download test fixtures")


# --- staleness --------------------------------------------------------------

NOW = datetime(2026, 10, 27, 13, 0, tzinfo=timezone.utc)


def stamp(days_ago: float) -> str:
    return (NOW - timedelta(days=days_ago)).isoformat().replace("+00:00", "Z")


def test_stale_runs() -> None:
    probs = staleness.stale_runs(
        {
            "fuzz.yml": {"createdAt": stamp(0.2), "url": "u"},
            "security.yml": {"createdAt": stamp(8.05), "url": "u"},
            "bench-regression.yml": None,
        },
        NOW,
        timedelta(days=8),
    )
    assert [p.detail.split(":")[0] for p in probs] == ["security.yml", "bench-regression.yml"]


def test_stale_issues_dedups_across_labels() -> None:
    issue = {"number": 677, "title": "t", "createdAt": stamp(12), "url": "u"}
    probs = staleness.stale_issues(
        [
            dict(issue, label="mutation-tests"),
            dict(issue, label="gate-canary"),
            {"number": 774, "title": "t", "createdAt": stamp(1), "url": "u", "label": "security-audit"},
        ],
        NOW,
        timedelta(days=7),
    )
    assert len(probs) == 1 and "#677" in probs[0].detail


def test_holdouts() -> None:
    holdouts = [
        {"target": "a", "issues": [1]},
        {"target": "b", "issues": [2, 3]},
        {"target": "c", "issues": [4]},
        {"target": "d", "issues": []},
    ]
    probs = staleness.holdout_problems(
        holdouts,
        matrix_targets=["x", "c"],
        all_targets={"a", "b", "c", "d", "x", "orphan"},
        issue_state={1: "CLOSED", 2: "CLOSED", 3: "OPEN", 4: "OPEN"},
    )
    text = "\n".join(map(str, probs))
    assert "hold-out a: #1 closed; re-enable" in text
    assert "hold-out b: #2 closed; drop the closed issue" in text
    assert "c is both in the nightly matrix and held out" in text
    assert "hold-out d links no issue" in text
    assert "orphan is in fuzz/fuzz_targets/ but neither" in text


def test_committed_fuzz_lists_agree_with_the_targets() -> None:
    holdouts, matrix = staleness.load_holdouts(ROOT / ".github/workflows/fuzz.yml")
    all_targets = {p.stem for p in (ROOT / "fuzz/fuzz_targets").glob("*.rs")}
    open_everywhere = {n: "OPEN" for h in holdouts for n in h["issues"]}
    assert staleness.holdout_problems(holdouts, matrix, all_targets, open_everywhere) == []


# --- mutation gate ----------------------------------------------------------


def test_plan_splits_shards_by_package() -> None:
    out = mutation_gate.plan([("core", "a.toml", 30), ("cli", "b.toml", 6)], per_shard=12, max_shards=4)
    assert out["planned"] == 36
    pkgs = [e["package"] for e in out["matrix"]["include"]]
    assert pkgs.count("core") == 2 and pkgs.count("cli") == 1
    assert {e["shard_count"] for e in out["matrix"]["include"] if e["package"] == "core"} == {2}


def test_plan_caps_and_keeps_every_package() -> None:
    out = mutation_gate.plan([("core", "a", 500), ("cli", "b", 1)], per_shard=12, max_shards=4)
    pkgs = [e["package"] for e in out["matrix"]["include"]]
    assert len(pkgs) == 4 and pkgs.count("cli") == 1


def test_plan_empty() -> None:
    out = mutation_gate.plan([("core", "a", 0), ("cli", "b", 0)], per_shard=12, max_shards=4)
    assert out == {"planned": 0, "shard_count": 0, "matrix": {"include": []}}


def test_unannotated(tmp_path: Path) -> None:
    src = tmp_path / "crates/cli/src/main.rs"
    src.parent.mkdir(parents=True)
    src.write_text(
        "fn a() -> bool {\n"
        "    true\n"
        "}\n"
        "// mutants: equivalent #123 (only changes the log line)\n"
        "fn b() {}\n"
        "fn c() -> u8 { 1 } // mutants: equivalent #9\n"
        "fn d() -> u8 { 2 } // mutants: equivalent\n"
    )
    missed = [
        "crates/cli/src/main.rs:2:5: replace a -> bool with false",
        "crates/cli/src/main.rs:5:10: replace b with ()",
        "crates/cli/src/main.rs:6:16: replace c -> u8 with 0",
        "crates/cli/src/main.rs:7:16: replace d -> u8 with 0",
        "garbage line",
    ]
    assert mutation_gate.unannotated(missed, tmp_path) == [missed[0], missed[3], missed[4]]


def test_check_cli(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    empty = tmp_path / "missed.txt"
    empty.write_text("")
    assert mutation_gate.main(["check", str(empty), "--root", str(tmp_path)]) == 0
    assert mutation_gate.main(["check", str(tmp_path / "absent.txt")]) == 0
