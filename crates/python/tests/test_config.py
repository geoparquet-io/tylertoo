"""Core's option rules fire through Python as ``tylertoo.ConfigError`` (#779).

Each rule below used to be checked by the CLI, by the bindings, or by
both, each with its own copy. Core owns them now; these tests show the
Python surface raises the same rule, as the same class, before reading
any input (every input below is missing on purpose). The CLI side is
``crates/cli/tests/config_rules.rs``.
"""

from __future__ import annotations

import math
import os
from pathlib import Path

import pytest

import tylertoo

MISSING = "/nonexistent/tylertoo-779/never-read.parquet"


def _convert(tmp_path: Path, **kwargs: object) -> None:
    # `max_zoom="auto"` reads the input to pick a zoom, so a rule checked
    # after it would surface as the missing input's RuntimeError instead.
    tylertoo.convert(
        MISSING,
        str(tmp_path / "out.pmtiles"),
        max_zoom="auto",
        **kwargs,  # type: ignore[arg-type]
    )


@pytest.mark.parametrize(
    ("kwargs", "needle"),
    [
        # #433 (1e37cbe): the export knobs that need no file.
        ({"tile_buffer": 257}, "--tile-buffer 257"),
        ({"extent": 0}, "extent 0"),
        # #427 (492502f): the spill directory.
        ({"spill_dir": "/nonexistent/tylertoo-779"}, "not an existing directory"),
        # #779: the reuse and write ends of a convert plan.
        (
            {"save_plan": "/tmp/a.plan", "plan": "/tmp/b.plan"},
            "--save-plan and --plan are mutually exclusive",
        ),
        # #779: a bbox that selects nothing.
        ({"bbox": (3.0, 0.0, 1.0, 1.0)}, "xmin <= xmax"),
        # #428 (b8fa58a): a NaN class rank.
        ({"class_rank_column": "c", "class_ranks": {"a": math.nan}}, "finite"),
        # Q1: a sort key with a class ranking.
        (
            {"sort_key": "pop", "class_rank_column": "c", "class_ranks": {"a": 1.0}},
            "mutually exclusive",
        ),
        # Q4: accumulation without clustering.
        ({"accumulate_attributes": {"pop": "sum"}}, "--cluster"),
        # #279/#745: two collapse switches.
        ({"collapse": True, "no_collapse": True}, "mutually exclusive"),
        # #364: both ladder spellings.
        ({"magnitude_ladder": "pop", "entry_zoom": "pop:1=4"}, "mutually exclusive"),
        # #386/#443: the feature id excluded by the property selection.
        ({"exclude_properties": ["id"], "feature_id": "id"}, "--feature-id"),
        # #433: the kept overview's directory.
        ({"keep_overview": "/nonexistent/tylertoo-779/o.parquet"}, "does not exist"),
    ],
)
def test_convert_refuses_each_rule_before_reading(
    tmp_path: Path, kwargs: dict[str, object], needle: str
) -> None:
    with pytest.raises(tylertoo.ConfigError, match=needle):
        _convert(tmp_path, **kwargs)
    assert not (tmp_path / "out.pmtiles").exists()


def test_config_error_is_a_value_error() -> None:
    # Existing `except ValueError` handlers keep working.
    assert issubclass(tylertoo.ConfigError, ValueError)


def test_overview_refuses_explicit_coalescing_in_partitioning_mode(
    tmp_path: Path,
) -> None:
    # Q3: formerly a CLI-only check; Python's coalesce_lines=True was inert.
    with pytest.raises(tylertoo.ConfigError, match="--coalesce-lines requires"):
        tylertoo.overview(
            MISSING,
            str(tmp_path / "o.parquet"),
            mode="partitioning",
            coalesce_lines=True,
        )


def test_export_refuses_a_bad_spill_dir_before_reading(tmp_path: Path) -> None:
    # #427: the CLI's `check_spill_dir`, now core's.
    with pytest.raises(tylertoo.ConfigError, match="not an existing directory"):
        tylertoo.export_pmtiles(
            MISSING,
            str(tmp_path / "out.pmtiles"),
            spill_dir=tmp_path / "nope",
        )


def test_pyramid_refuses_a_bad_option_before_tiling(tmp_path: Path) -> None:
    band = tmp_path / "band.parquet"
    band.write_bytes(b"not read")
    with pytest.raises(tylertoo.ConfigError, match="--tile-buffer 999"):
        tylertoo.pyramid(
            [f"0-2:{band}"], str(tmp_path / "out.pmtiles"), tile_buffer=999
        )


@pytest.mark.skipif(
    not hasattr(os, "geteuid") or os.geteuid() == 0,
    reason="root ignores directory modes",
)
def test_a_read_only_spill_dir_is_refused_before_reading(tmp_path: Path) -> None:
    # #779: formerly "Permission denied" (RuntimeError) when the spill first
    # opened, deep into the run.
    ro = tmp_path / "ro"
    ro.mkdir()
    ro.chmod(0o555)
    try:
        with pytest.raises(tylertoo.ConfigError, match="is not writable"):
            _convert(tmp_path, spill_dir=str(ro))
    finally:
        ro.chmod(0o755)


def test_one_message_for_a_nan_class_rank(tmp_path: Path) -> None:
    # #428: the CLI's `--class-rank c:a=nan` and Python's dict spelling are
    # refused by one core rule with one message.
    with pytest.raises(tylertoo.ConfigError) as err:
        _convert(tmp_path, class_rank_column="c", class_ranks={"a": math.nan})
    assert str(err.value).startswith('rank NaN for class "a" must be a finite number')
