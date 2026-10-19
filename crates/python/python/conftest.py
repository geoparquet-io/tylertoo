"""Fixtures for the docstring examples (`--doctest-modules`).

The examples name `buildings.parquet`, `overview.parquet` and
`buildings.pmtiles`. Each one runs in a fresh directory that holds all
three: a small committed polygon fixture, and an overview file and an
archive built from it once per session.
"""

from __future__ import annotations

import shutil
from pathlib import Path

import pytest

import tylertoo

FIXTURE = (
    Path(__file__).resolve().parents[3]
    / "tests"
    / "fixtures"
    / "streaming"
    / "multi-rowgroup-small.parquet"
)


@pytest.fixture(scope="session")
def _example_files(tmp_path_factory: pytest.TempPathFactory) -> Path:
    source = tmp_path_factory.mktemp("doctest-inputs")
    shutil.copy(FIXTURE, source / "buildings.parquet")
    tylertoo.overview(
        str(source / "buildings.parquet"),
        str(source / "overview.parquet"),
        max_zoom=10,
    )
    tylertoo.export_pmtiles(
        str(source / "overview.parquet"),
        str(source / "buildings.pmtiles"),
    )
    return source


@pytest.fixture(autouse=True)
def _example_directory(
    _example_files: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    for name in ("buildings.parquet", "overview.parquet", "buildings.pmtiles"):
        shutil.copy(_example_files / name, tmp_path / name)
    monkeypatch.chdir(tmp_path)
