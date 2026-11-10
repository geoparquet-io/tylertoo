"""Tests for `tylertoo.stats()`: the `tylertoo stats` report (#825)."""

import json
import tempfile
from pathlib import Path
from typing import Any

import pytest

import tylertoo

REPO = Path(__file__).parent.parent.parent.parent
FIXTURES_DIR = REPO / "tests" / "fixtures"
ROADS_PMTILES = FIXTURES_DIR / "golden" / "road-detections.pmtiles"
BUILDINGS_PMTILES = FIXTURES_DIR / "golden" / "open-buildings.pmtiles"
CLI_SNAPSHOT = REPO / "crates" / "cli" / "tests" / "cmd" / "stats.md"


def cli_json(flags: str) -> dict[str, Any]:
    """The JSON the CLI printed for `stats <golden> <flags>`, as pinned by
    the trycmd snapshot."""
    text = CLI_SNAPSHOT.read_text()
    command = f"road-detections.pmtiles {flags}\n"
    start = text.index(command) + len(command)
    end = text.index("```", start)
    report: dict[str, Any] = json.loads(text[start:end])
    return report


class TestStatsApi:
    def test_stats_is_exported(self) -> None:
        assert "stats" in tylertoo.__all__
        assert callable(tylertoo.stats)

    def test_options_are_keyword_only(self) -> None:
        with pytest.raises(TypeError):
            tylertoo.stats(str(ROADS_PMTILES), 2)  # type: ignore[misc]

    def test_unknown_keyword_is_a_config_error(self) -> None:
        with pytest.raises(tylertoo.ConfigError, match="unknown keyword"):
            tylertoo.stats(str(ROADS_PMTILES), json=True)  # type: ignore[call-arg]

    def test_missing_archive_raises_runtime_error(self) -> None:
        with (
            tempfile.TemporaryDirectory() as tmpdir,
            pytest.raises(RuntimeError, match="stats failed"),
        ):
            tylertoo.stats(str(Path(tmpdir) / "missing.pmtiles"))


class TestStatsMatchesCli:
    def test_equals_cli_json(self) -> None:
        assert tylertoo.stats(str(ROADS_PMTILES), largest=2) == cli_json(
            "--largest 2 --json"
        )

    def test_layers_equals_cli_json(self) -> None:
        report = tylertoo.stats(str(ROADS_PMTILES), largest=0, layers=True)
        assert report == cli_json("--largest 0 --layers --json")

    def test_per_layer_only_when_asked(self) -> None:
        assert "per_layer" not in tylertoo.stats(str(ROADS_PMTILES))
        assert len(tylertoo.stats(str(ROADS_PMTILES))["largest"]) == 10


def test_layers_on_a_multi_layer_pyramid() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "multi.pmtiles"
        tylertoo.pyramid(
            [f"0-4:{ROADS_PMTILES}:roads", f"5-8:{BUILDINGS_PMTILES}:buildings"],
            str(out),
        )
        rows = tylertoo.stats(str(out), layers=True)["per_layer"]
    layers = {(r["z"], r["layer"]) for r in rows}
    assert {z for z, _ in layers} == set(range(9))
    assert {name for z, name in layers if z <= 4} == {"road-detections"}
    assert {name for z, name in layers if z >= 5} == {"open-buildings"}
    for r in rows:
        assert sum(r["geometry_types"].values()) == r["feature_count"] > 0
        assert r["bytes"] > 0
