"""verify_fixtures.sh: every fixtures-v1 asset must match benchmarks/fixtures.sha256."""

import hashlib
import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "scripts" / "ci" / "verify_fixtures.sh"
PINS = ROOT / "benchmarks" / "fixtures.sha256"
RELEASE_ASSETS = {
    "fieldmaps-boundaries.parquet", "fieldmaps-madagascar-adm4.parquet",
    "ftw-moldova-z10.parquet", "open-buildings.parquet", "road-detections.parquet",
    "wkt-encoded.parquet",
}


def _verify(fixtures: Path, pins: Path) -> subprocess.CompletedProcess:
    return subprocess.run(["bash", str(SCRIPT), str(fixtures), str(pins)],
                          capture_output=True, text=True, check=False)


@pytest.fixture
def pinned(tmp_path):
    """Two files and a pin list that matches them."""
    d = tmp_path / "fx"
    d.mkdir()
    lines = []
    for name, body in (("a.parquet", b"PAR1 a"), ("b.parquet", b"PAR1 b")):
        (d / name).write_bytes(body)
        lines.append(f"{hashlib.sha256(body).hexdigest()}  {name}\n")
    pins = tmp_path / "pins.sha256"
    pins.write_text("".join(lines))
    return d, pins


def test_matching_files_pass(pinned):
    d, pins = pinned
    res = _verify(d, pins)
    assert res.returncode == 0, res.stdout + res.stderr


def test_one_corrupted_byte_fails(pinned):
    d, pins = pinned
    body = bytearray((d / "a.parquet").read_bytes())
    body[0] ^= 0xFF
    (d / "a.parquet").write_bytes(bytes(body))
    res = _verify(d, pins)
    assert res.returncode != 0
    assert "a.parquet" in res.stdout + res.stderr


def test_a_missing_file_fails(pinned):
    d, pins = pinned
    (d / "b.parquet").unlink()
    assert _verify(d, pins).returncode != 0


def test_a_missing_directory_fails(tmp_path, pinned):
    _, pins = pinned
    assert _verify(tmp_path / "nope", pins).returncode != 0


def test_the_committed_pins_cover_every_release_asset():
    names = {line.split()[1] for line in PINS.read_text().splitlines() if line.strip()}
    assert names == RELEASE_ASSETS
    for line in PINS.read_text().splitlines():
        sha, _ = line.split()
        assert len(sha) == 64 and set(sha) <= set("0123456789abcdef")  # noqa: PLR2004


def test_the_committed_pins_match_the_lfs_copies():
    """The LFS pointers in tests/fixtures/realdata carry the same sha256."""
    lfs = ROOT / "tests" / "fixtures" / "realdata"
    for line in PINS.read_text().splitlines():
        sha, name = line.split()
        p = lfs / name
        if not p.exists():
            continue  # wkt-encoded.parquet is release-only
        text = p.read_bytes()[:200]
        if text.startswith(b"version https://git-lfs"):
            assert f"oid sha256:{sha}".encode() in text, name
        else:
            assert hashlib.sha256(p.read_bytes()).hexdigest() == sha, name


def test_the_real_release_download_verifies(tmp_path):
    """Opt-in: when the assets are on disk (FIXTURES_DIR), they verify."""
    import os
    src = os.environ.get("FIXTURES_DIR")
    if not src:
        pytest.skip("set FIXTURES_DIR to a `gh release download fixtures-v1` directory")
    d = tmp_path / "fx"
    shutil.copytree(src, d)
    res = _verify(d, PINS)
    assert res.returncode == 0, res.stdout + res.stderr
