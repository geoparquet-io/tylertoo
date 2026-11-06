"""Hypothesis over the option surface (#677; replaces mutation testing).

Core owns option validation. The oracle below is the documented range of
each option (docs/OVERVIEW_TUNING.md, the CLI help, core's own tests), not
core's code, so a loosened bound fails: a refused value raises
``tylertoo.ConfigError`` (a ``ValueError``) naming the argument before the
input is read; an accepted value fails later, on the missing input.
"""

from __future__ import annotations

import math
import tempfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from hypothesis import given, settings
from hypothesis import strategies as st

import tylertoo

MISSING = "/nonexistent/tylertoo-677/never-read.parquet"
FIXTURE = Path(__file__).parents[3] / "tests/fixtures/realdata/open-buildings.parquet"


def _finite_ge(low: float) -> Callable[[float], bool]:
    return lambda v: math.isfinite(v) and v >= low


# keyword -> (strategy, name in the message, documented-valid, documented-invalid)
_INTS = st.integers(min_value=-5, max_value=2_000_000)
_FLOATS = st.one_of(st.floats(), st.sampled_from([0.0, 0.5, 1.0, 2.0]))
_KNOBS: dict[str, Any] = {
    "read_batch_size": (
        _INTS,
        "read-batch-size",
        lambda v: 1 <= v <= 1_048_576,
        lambda v: not 1 <= v <= 1_048_576,
    ),
    "extent": (_INTS, "extent", lambda v: v >= 1, lambda v: v == 0),
    "simplify_factor": (
        _FLOATS,
        "simplify.factor",
        _finite_ge(0),
        lambda v: not math.isfinite(v),
    ),
    # Documented "exponent (>=1)"; only finiteness is enforced (FOLLOWUP-drop-gamma).
    "drop_gamma": (
        _FLOATS,
        "drop.gamma",
        _finite_ge(1),
        lambda v: not math.isfinite(v),
    ),
}
# Boundary values, so a loosened bound fails deterministically.
_BOUNDARIES: list[tuple[str, Any]] = [
    ("read_batch_size", v) for v in (0, 1, 2, 1_048_575, 1_048_576, 1_048_577)
] + [("extent", 0), ("extent", 1), ("extent", 4096)]
_BOUNDARIES += [
    (k, v)
    for k in ("simplify_factor", "drop_gamma")
    for v in (math.nan, math.inf, -math.inf)
]
_BOUNDARIES += [("simplify_factor", 0.0), ("drop_gamma", 1.0)]


def _outcome(keyword: str, value: object) -> BaseException:
    kwargs: dict[str, Any] = {keyword: value}
    with tempfile.TemporaryDirectory() as tmp:
        try:
            # `max_zoom="auto"` reads the input to pick a zoom, so a rule
            # checked after it would surface as the missing input's error.
            tylertoo.convert(
                MISSING, str(Path(tmp) / "o.pmtiles"), max_zoom="auto", **kwargs
            )
        except Exception as exc:
            return exc
    raise AssertionError("the input is missing, so something must fail")


def _check(keyword: str, value: Any) -> None:
    _, needle, valid, invalid = _KNOBS[keyword]
    exc = _outcome(keyword, value)
    refused = isinstance(exc, tylertoo.ConfigError)
    if valid(value):
        assert not refused, f"{keyword}={value!r} is documented valid: {exc}"
    if invalid(value):
        assert refused, f"{keyword}={value!r} is documented invalid: {exc!r}"
    if refused:
        assert isinstance(exc, ValueError)
        names = (keyword, keyword.replace("_", "-"), needle, needle.replace(".", "-"))
        assert any(n in str(exc) for n in names), f"{keyword}={value!r}: {exc}"
    else:
        assert isinstance(exc, RuntimeError), repr(exc)


@pytest.mark.parametrize(("keyword", "value"), _BOUNDARIES)
def test_documented_boundaries(keyword: str, value: Any) -> None:
    _check(keyword, value)


@settings(max_examples=40, deadline=None)
@given(data=st.data(), keyword=st.sampled_from(sorted(_KNOBS)))
def test_documented_ranges(data: st.DataObject, keyword: str) -> None:
    _check(keyword, data.draw(_KNOBS[keyword][0]))


@pytest.mark.xfail(
    strict=True, reason="FOLLOWUP-drop-gamma: documented >=1, not enforced"
)
@pytest.mark.parametrize("value", [0.0, 0.5, -1.0])
def test_drop_gamma_below_one_is_refused(value: float) -> None:
    assert isinstance(_outcome("drop_gamma", value), tylertoo.ConfigError)


@settings(max_examples=6, deadline=None)
@given(extent=st.integers(min_value=256, max_value=8192))
def test_accepted_extent_builds_an_archive(extent: int) -> None:
    """An accepted extent is not just unrefused: a real export honors it."""
    with tempfile.TemporaryDirectory() as tmp:
        ovr, pm = Path(tmp) / "o.parquet", Path(tmp) / "o.pmtiles"
        tylertoo.overview(
            str(FIXTURE), str(ovr), min_zoom=13, max_zoom=14, polygon_visibility=0.0
        )
        report = tylertoo.export_pmtiles(str(ovr), str(pm), extent=extent)
        assert report["total_tiles"] > 0
        assert pm.read_bytes()[:8] == b"PMTiles\x03"
