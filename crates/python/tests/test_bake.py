"""Tests for `tylertoo.bake()`, `tylertoo.strip()` and `convert(bake=...)`."""

from pathlib import Path

import pyarrow.parquet as pq  # type: ignore[import-untyped]
import pytest

import tylertoo

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
STREAMING_SMALL = FIXTURES_DIR / "streaming" / "multi-rowgroup-small.parquet"


@pytest.fixture(scope="module")
def archive(tmp_path_factory: pytest.TempPathFactory) -> Path:
    out = tmp_path_factory.mktemp("bake") / "small.pmtiles"
    tylertoo.convert(str(STREAMING_SMALL), str(out), max_zoom=8)
    return out


def test_bake_and_strip_are_exported() -> None:
    for name in ("bake", "strip", "BakeReport", "BakedArchive"):
        assert name in tylertoo.__all__


def test_bake_then_strip_round_trips(archive: Path, tmp_path: Path) -> None:
    baked = tmp_path / "baked.parquet"
    report = tylertoo.bake(str(STREAMING_SMALL), str(archive), str(baked))
    assert report["archive"]["length"] == archive.stat().st_size
    assert report["output_bytes"] == baked.stat().st_size

    original = pq.read_table(STREAMING_SMALL)
    assert pq.read_table(baked).equals(original)
    keys = pq.read_metadata(baked).metadata
    assert b"geo:pmtiles" in keys
    assert b"geo" in keys

    # The archive reads from the baked file as from the archive itself.
    assert tylertoo.stats(str(baked)) == tylertoo.stats(str(archive))

    plain = tmp_path / "plain.parquet"
    tylertoo.strip(str(baked), str(plain))
    assert plain.read_bytes() == STREAMING_SMALL.read_bytes()


def test_convert_bakes_a_copy_of_the_input(tmp_path: Path) -> None:
    out, baked = tmp_path / "out.pmtiles", tmp_path / "baked.parquet"
    tylertoo.convert(str(STREAMING_SMALL), str(out), max_zoom=8, bake=str(baked))
    assert out.is_file()
    assert tylertoo.stats(str(baked)) == tylertoo.stats(str(out))
    assert pq.read_table(baked).num_rows == pq.read_table(STREAMING_SMALL).num_rows


def test_convert_bake_refuses_to_overwrite_the_input(tmp_path: Path) -> None:
    with pytest.raises(tylertoo.ConfigError, match="would overwrite"):
        tylertoo.convert(
            str(STREAMING_SMALL),
            str(tmp_path / "out.pmtiles"),
            max_zoom=8,
            bake=str(STREAMING_SMALL),
        )


def test_errors_are_classified(archive: Path, tmp_path: Path) -> None:
    baked = tmp_path / "baked.parquet"
    tylertoo.bake(str(STREAMING_SMALL), str(archive), str(baked))
    with pytest.raises(ValueError, match="already holds a baked"):
        tylertoo.bake(str(baked), str(archive), str(tmp_path / "again.parquet"))
    with pytest.raises(ValueError, match="holds no baked"):
        tylertoo.strip(str(STREAMING_SMALL), str(tmp_path / "x.parquet"))
    with pytest.raises(RuntimeError):
        tylertoo.bake(
            str(STREAMING_SMALL), str(STREAMING_SMALL), str(tmp_path / "y.parquet")
        )
