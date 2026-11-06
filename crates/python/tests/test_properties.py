"""Hypothesis over the option surface (#677; replaces mutation testing).

Core owns option validation, so the invariant is stated at the Python
boundary: a refused value raises ``tylertoo.ConfigError`` (a ``ValueError``)
naming the argument, before the input is read; an accepted value gets past
validation and fails later, on the missing input, as something else.
"""

from __future__ import annotations

import math
import tempfile
from pathlib import Path
from typing import Any

from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

import tylertoo

MISSING = "/nonexistent/tylertoo-677/never-read.parquet"

# keyword -> (strategy, error-text needle, value is certainly accepted?)
_SIZES = st.integers(min_value=-5, max_value=2_000_000)
_FLOATS = st.one_of(
    st.floats(allow_nan=True, allow_infinity=True),
    st.sampled_from([0.0, 1.0, 2.0]),
)
_KNOBS = {
    "read_batch_size": (_SIZES, "read-batch-size", lambda v: 1 <= v <= 1_048_576),
    "extent": (
        st.integers(min_value=0, max_value=70_000),
        "extent",
        lambda v: 256 <= v <= 8192,
    ),
    "simplify_factor": (
        _FLOATS,
        "simplify.factor",
        lambda v: math.isfinite(v) and v > 0,
    ),
    "drop_gamma": (_FLOATS, "drop.gamma", lambda v: math.isfinite(v) and v > 0),
}


def _outcome(keyword: str, value: object) -> BaseException | None:
    kwargs: dict[str, Any] = {keyword: value}
    with tempfile.TemporaryDirectory() as tmp:
        try:
            # `max_zoom="auto"` reads the input to pick a zoom, so a rule
            # checked after it would surface as the missing input's error.
            tylertoo.convert(
                MISSING,
                str(Path(tmp) / "out.pmtiles"),
                max_zoom="auto",
                **kwargs,
            )
        except Exception as exc:
            return exc
    return None


@settings(max_examples=40, deadline=None, suppress_health_check=[HealthCheck.too_slow])
@given(data=st.data(), keyword=st.sampled_from(sorted(_KNOBS)))
def test_refused_values_name_the_argument_and_accepted_ones_pass_validation(
    data: st.DataObject, keyword: str
) -> None:
    strategy, needle, surely_ok = _KNOBS[keyword]
    value = data.draw(strategy)
    exc = _outcome(keyword, value)
    assert exc is not None  # the input is missing, so something must fail
    if surely_ok(value):  # the converse: a clearly valid value is never refused
        assert not isinstance(exc, tylertoo.ConfigError), f"{keyword}={value!r}: {exc}"
    if isinstance(exc, tylertoo.ConfigError):
        assert isinstance(exc, ValueError)
        assert not surely_ok(value), f"{keyword}={value!r} refused: {exc}"
        names = (keyword, keyword.replace("_", "-"), needle, needle.replace(".", "-"))
        assert any(n in str(exc) for n in names), (
            f"{keyword}={value!r} refusal names nothing: {exc}"
        )
    else:
        assert isinstance(exc, RuntimeError), repr(exc)
        # Past validation: the value was one the knob accepts.
        assert keyword != "extent" or value != 0
        if keyword in {"simplify_factor", "drop_gamma"}:
            assert math.isfinite(value)
        if keyword == "read_batch_size":
            assert 1 <= value <= 1_048_576


BUILDINGS = (
    Path(__file__).parents[3] / "tests" / "fixtures" / "realdata" / "open-buildings.parquet"
)


@settings(max_examples=6, deadline=None)
@given(extent=st.integers(min_value=256, max_value=8192))
def test_accepted_extent_builds_an_archive(extent: int) -> None:
    """An accepted extent is not just unrefused: a real export honors it."""
    with tempfile.TemporaryDirectory() as tmp:
        ovr, pm = Path(tmp) / "o.parquet", Path(tmp) / "o.pmtiles"
        tylertoo.overview(
            str(BUILDINGS), str(ovr), min_zoom=13, max_zoom=14, polygon_visibility=0.0
        )
        report = tylertoo.export_pmtiles(str(ovr), str(pm), extent=extent)
        assert report["total_tiles"] > 0
        assert pm.read_bytes()[:8] == b"PMTiles\x03"
