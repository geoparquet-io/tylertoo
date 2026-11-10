"""`python -m tylertoo` runs the CLI the binary does (#819).

Each test runs the entry point as a subprocess, the way it is meant to be
used: exit codes, stderr logging, and signals are process-level behavior.
The CLI's output itself is pinned by the trycmd snapshots in crates/cli,
which CI also runs against the `tylertoo` script this package installs.
"""

import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest

FIXTURES_DIR = Path(__file__).parent.parent.parent.parent / "tests" / "fixtures"
NAMES = FIXTURES_DIR / "properties" / "overture-names.parquet"
POSIX = pytest.mark.skipif(sys.platform == "win32", reason="POSIX pipes and FIFOs")


def tylertoo(*args: str, **kwargs) -> subprocess.CompletedProcess[str]:
    env = {**os.environ, "NO_COLOR": "1", **kwargs.pop("env", {})}
    return subprocess.run(
        [sys.executable, "-m", "tylertoo", *args],
        capture_output=True,
        text=True,
        timeout=120,
        env=env,
        check=False,
        **kwargs,
    )


def test_help_and_version_exit_0() -> None:
    help_ = tylertoo("--help")
    assert help_.returncode == 0, help_.stderr
    assert help_.stdout.startswith("Convert GeoParquet to PMTiles")
    assert "- 101: a panic" in help_.stdout
    version = tylertoo("--version")
    assert version.returncode == 0
    assert re.fullmatch(r"tylertoo \d+\.\d+\.\d+\S*\n", version.stdout)


@pytest.mark.parametrize(
    "args",
    [
        ["tiles"],
        ["--no-such-flag"],
        ["-q", "tiles", "in.parquet", "out.pmtiles", "--verbose"],
    ],
)
def test_a_rejected_command_line_exits_2(args: list[str]) -> None:
    result = tylertoo(*args)
    assert result.returncode == 2, result.stderr
    # The program name is the CLI's, not __main__.py.
    assert "Usage: tylertoo" in result.stderr


def test_a_failed_run_exits_1(tmp_path: Path) -> None:
    result = tylertoo("stats", str(tmp_path / "missing.pmtiles"))
    assert result.returncode == 1
    assert result.stderr.startswith("Error: ")


def test_tiles_writes_an_archive_and_logs_like_the_binary(tmp_path: Path) -> None:
    out = tmp_path / "names.pmtiles"
    result = tylertoo(
        "tiles", str(NAMES), str(out), "--max-zoom", "6", env={"RUST_LOG": "info"}
    )
    assert result.returncode == 0, result.stderr
    assert "→ " in result.stdout
    # env_logger's format, the binary's, not Python logging's.
    assert re.search(r"^\[\S+ INFO +tylertoo_core", result.stderr, re.MULTILINE)
    assert out.read_bytes()[:8] == b"PMTiles\x03"

    stats = tylertoo("stats", str(out), "--json")
    assert stats.returncode == 0, stats.stderr
    assert json.loads(stats.stdout)["per_zoom"]

    quiet = tylertoo("-q", "tiles", str(NAMES), str(tmp_path / "q.pmtiles"))
    assert quiet.returncode == 0, quiet.stderr
    assert quiet.stdout == ""
    assert " INFO " not in quiet.stderr


@POSIX
def test_a_panic_exits_101(tmp_path: Path) -> None:
    out = tmp_path / "names.pmtiles"
    assert tylertoo("-q", str(NAMES), str(out)).returncode == 0
    # A stdout whose reader is gone: `println!` panics, as in the binary.
    read, write = os.pipe()
    os.close(read)
    try:
        result = subprocess.run(
            [sys.executable, "-m", "tylertoo", "stats", str(out)],
            stdout=write,
            stderr=subprocess.PIPE,
            text=True,
            timeout=120,
            check=False,
        )
    finally:
        os.close(write)
    assert result.returncode == 101, result.stderr
    assert "panicked" in result.stderr
    assert "PanicException" not in result.stderr


@POSIX
@pytest.mark.parametrize("signum", [signal.SIGINT, signal.SIGTERM])
def test_a_signal_stops_a_run_blocked_in_rust(tmp_path: Path, signum: int) -> None:
    # The CLI blocks reading its --files-from manifest, a FIFO nobody writes,
    # so the process is inside Rust, where Python never checks its signal
    # flags. Only the default action can stop it.
    fifo = tmp_path / "manifest"
    os.mkfifo(fifo)
    proc = subprocess.Popen(
        [
            sys.executable,
            "-m",
            "tylertoo",
            "tiles",
            "--files-from",
            str(fifo),
            str(tmp_path / "out.pmtiles"),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    writer = None
    try:
        # A non-blocking open for writing succeeds once the CLI has the
        # FIFO open for reading; it then waits in read() for our data.
        deadline = time.monotonic() + 60
        while writer is None:
            try:
                writer = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
            except OSError:
                assert proc.poll() is None, proc.communicate()[1]
                assert time.monotonic() < deadline, "the CLI never opened the FIFO"
                time.sleep(0.05)
        proc.send_signal(signum)
        assert proc.wait(timeout=10) == -signum
    finally:
        proc.kill()
        proc.communicate()
        if writer is not None:
            os.close(writer)


def test_the_console_script_runs_the_cli() -> None:
    bindir = Path(sys.executable).parent
    script = bindir / ("tylertoo.exe" if sys.platform == "win32" else "tylertoo")
    result = subprocess.run(
        [str(script), "--version"],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.startswith("tylertoo ")
