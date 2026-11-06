"""corpus/fetch.sh checks every output against its pin, cached or not (#810).

Runs a copy of fetch.sh against a one-dataset manifest with stub duckdb,
gpio, curl and unzip, so no network or real tools are needed.
"""

import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
ZIP = b"PK fake natural earth zip"


@pytest.fixture
def corpus(tmp_path):
    """A corpus dir with fetch.sh, a pinned Natural Earth dataset of 3 rows,
    and stub tools; returns (dir, env)."""
    if not shutil.which("jq"):
        pytest.skip("needs jq")
    c = tmp_path / "corpus"
    c.mkdir()
    shutil.copy(ROOT / "corpus" / "fetch.sh", c / "fetch.sh")
    (c / "manifest.json").write_text(json.dumps({
        "overture": {"release": "r", "bucket": "s3://b", "region": "x"},
        "datasets": [{"id": "ne", "profile": "default", "source": "natural-earth",
                      "url": "https://example.invalid/ne.zip", "shapefile": "ne.shp",
                      "expected_features": 3, "sha256": hashlib.sha256(ZIP).hexdigest()}]}))
    stubs = tmp_path / "bin"
    stubs.mkdir()
    (tmp_path / "zip").write_bytes(ZIP)
    for name, body in {
        "duckdb": 'cat "$STUB_ROWS"',
        "gpio": 'for last; do :; done; echo parquet > "$last"',
        "curl": f'while [ $# -gt 0 ]; do [ "$1" = -o ] && cp {tmp_path}/zip "$2"; shift; done',
        "unzip": "true",
    }.items():
        (stubs / name).write_text(f"#!/bin/bash\n{body}\n")
        (stubs / name).chmod(0o755)
    rows = tmp_path / "rows"
    rows.write_text("3\n")
    env = {**os.environ, "PATH": f"{stubs}:{os.environ['PATH']}", "STUB_ROWS": str(rows)}
    return c, env


def _fetch(c: Path, env: dict) -> subprocess.CompletedProcess:
    return subprocess.run(["bash", str(c / "fetch.sh")], env=env, capture_output=True,
                          text=True, check=False)


def test_fresh_and_cached_outputs_pass(corpus):
    c, env = corpus
    for _ in range(2):  # the second run uses the cache
        res = _fetch(c, env)
        assert res.returncode == 0, res.stdout + res.stderr
        assert "pin ok [ne]" in res.stdout


def test_a_cached_output_with_a_corrupt_zip_fails(corpus):
    c, env = corpus
    assert _fetch(c, env).returncode == 0
    (c / "data" / "raw" / "ne.zip").write_bytes(b"corrupt")
    res = _fetch(c, env)
    assert res.returncode == 1
    assert "ne.zip has sha256" in res.stderr


def test_a_stale_cached_output_fails_on_its_row_count(corpus):
    c, env = corpus
    assert _fetch(c, env).returncode == 0
    Path(env["STUB_ROWS"]).write_text("2\n")
    res = _fetch(c, env)
    assert res.returncode == 1
    assert "has 2 rows, manifest.json pins 3" in res.stderr


def test_a_cache_without_its_zip_fails(corpus):
    c, env = corpus
    raw = c / "data" / "raw"
    raw.mkdir(parents=True)
    (raw / "ne.parquet").write_text("old cache")
    res = _fetch(c, env)
    assert res.returncode == 1
    assert "--force" in res.stderr
