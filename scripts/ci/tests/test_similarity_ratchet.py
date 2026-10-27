"""Tests for scripts/ci/similarity_ratchet.py and rust_source.py (#779).

Run: uv run --no-project --with pytest pytest scripts/ci/tests -q
"""

from __future__ import annotations

from pathlib import Path

import similarity_ratchet as sr
from rust_source import strip_test_items, cfg_test_lines

OUTPUT = """\
Analyzing Rust code similarity...

=== Function Similarity ===
Checking 2 files for duplicates...

Duplicates in crates/core/src/mvt.rs:
------------------------------------------------------------
  crates/core/src/mvt.rs:10-40 function segments_meet <-> crates/core/src/mvt.rs:50-80 function segments_cross_or_overlap
  Similarity: 91.37%
  crates/core/src/overview/assign.rs:5-30 method default <-> crates/core/src/overview/assign.rs:60-90 method default
  Classes: A <-> B
  Similarity: 93.60%

Total duplicate pairs found: 2
"""


def test_parse_pairs_reads_names_lines_and_similarity() -> None:
    pairs = sr.parse_pairs(OUTPUT)
    assert len(pairs) == 2
    sim, fa, la, na, fb, lb, nb = pairs[0]
    assert (round(sim, 4), fa, la, na, lb, nb) == (
        0.9137,
        "crates/core/src/mvt.rs",
        10,
        "segments_meet",
        50,
        "segments_cross_or_overlap",
    )
    assert pairs[1][3] == pairs[1][6] == "default", "a Classes: line between is skipped"


def test_in_file_keys_sort_names_and_drop_line_numbers() -> None:
    assert sr.in_file_keys(OUTPUT) == [
        "crates/core/src/mvt.rs :: segments_cross_or_overlap <-> segments_meet",
        "crates/core/src/overview/assign.rs :: default <-> default",
    ]


def test_cross_engine_keys_keep_only_pairs_spanning_both_engines() -> None:
    out = """\
  engines.rs:10-30 function visit <-> engines.rs:200-230 function visit
  Similarity: 95.59%
  engines.rs:10-30 function a <-> engines.rs:40-70 function b
  Similarity: 90.00%
  engines.rs:150-170 function c <-> engines.rs:20-40 function d
  Similarity: 88.00%
"""
    assert sr.cross_engine_keys(out, split_line=100) == [
        "convert.rs::d <-> stream.rs::c",
        "convert.rs::visit <-> stream.rs::visit",
    ]


PINS = {"similarity_pairs": 2, "similarity_convert_stream": 1}
PINNED = (["f :: a <-> b", "f :: c <-> d"], ["convert.rs::x <-> stream.rs::x"])


def test_check_passes_when_current_equals_baseline_and_pins() -> None:
    assert sr.check(PINNED, PINNED, PINS) == []


def test_check_fails_on_a_new_pair() -> None:
    current = (["f :: a <-> b", "f :: c <-> d", "f :: e <-> g"], PINNED[1])
    msgs = sr.check(current, PINNED, PINS)
    assert len(msgs) == 1 and "new near-duplicate" in msgs[0] and "e <-> g" in msgs[0]


def test_check_fails_when_a_pinned_pair_is_gone_so_the_pin_drops() -> None:
    current = (PINNED[0], [])
    msgs = sr.check(current, PINNED, PINS)
    assert len(msgs) == 1 and "no longer reported" in msgs[0] and "lower the pin" in msgs[0]


def test_check_counts_duplicate_keys() -> None:
    """Two `default <-> default` pairs in one file are two findings."""
    pinned = (["f :: default <-> default"], [])
    current = (["f :: default <-> default", "f :: default <-> default"], [])
    assert sr.check(current, pinned, {"similarity_pairs": 1, "similarity_convert_stream": 0})


def test_check_fails_when_the_pin_disagrees_with_the_baseline() -> None:
    msgs = sr.check(PINNED, PINNED, {"similarity_pairs": 3, "similarity_convert_stream": 1})
    assert len(msgs) == 1 and "set the pin to 2" in msgs[0]


def test_baseline_round_trips(tmp_path: Path) -> None:
    path = tmp_path / "baseline.txt"
    sr.write_baseline(path, PINNED[0], PINNED[1], (0.85, 0.80))
    assert sr.read_baseline(path) == PINNED


def test_committed_baseline_matches_the_pins() -> None:
    """The committed files agree with each other (similarity-rs not needed)."""
    _, pins = sr.load_config()
    in_file, cross = sr.read_baseline(sr.BASELINE)
    assert (len(in_file), len(cross)) == (pins["similarity_pairs"], pins["similarity_convert_stream"])


RUST = """\
fn prod() {
    body();
}

#[cfg(test)]
mod tests {
    fn helper() {
        x();
    }
}

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(all(test, feature = "remote"))]
#[allow(dead_code)]
fn remote_only(
    a: u8,
) {
}

fn after() {}
"""


def test_test_item_lines_cover_modules_declarations_and_multiline_items() -> None:
    lines = cfg_test_lines(RUST)
    src = RUST.splitlines()
    kept = [src[n - 1] for n in range(1, len(src) + 1) if n not in lines]
    assert "fn prod() {" in kept and "fn after() {}" in kept
    assert not any("helper" in k or "testutil" in k or "remote_only" in k for k in kept)


def test_strip_test_items_keeps_production_text() -> None:
    out = strip_test_items(RUST)
    assert "fn prod()" in out and "fn after()" in out and "mod tests" not in out
