"""Tests for `tylertoo.serve()`: the `tylertoo serve` server (#825)."""

import contextlib
import signal
import socket
import subprocess
import sys
import time
import urllib.request
from collections.abc import Iterator
from pathlib import Path

import pytest

import tylertoo

REPO = Path(__file__).parent.parent.parent.parent
ROADS_PMTILES = REPO / "tests" / "fixtures" / "golden" / "road-detections.pmtiles"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port: int = s.getsockname()[1]
        return port


def fetch(url: str, headers: dict[str, str] | None = None) -> bytes:
    """GET `url`, retrying while the server starts."""
    for _ in range(100):
        try:
            req = urllib.request.Request(url, headers=headers or {})
            with urllib.request.urlopen(req, timeout=5) as r:
                body: bytes = r.read()
                return body
        except OSError:
            time.sleep(0.05)
    raise AssertionError(f"{url} never answered")


@contextlib.contextmanager
def serving(
    archives: str | list[str], **kwargs: object
) -> Iterator[subprocess.Popen[str]]:
    """`tylertoo.serve` in a child process, killed on exit.

    Not a daemon thread: `serve` polls Python's signal handlers, and a
    thread still in that loop when the interpreter shuts down crashes it.
    """
    code = f"import tylertoo; tylertoo.serve({archives!r}, **{kwargs!r})"
    proc = subprocess.Popen(
        [sys.executable, "-c", code], stderr=subprocess.PIPE, text=True
    )
    try:
        yield proc
    finally:
        proc.kill()
        proc.wait()


class TestServe:
    def test_serve_is_exported(self) -> None:
        assert "serve" in tylertoo.__all__

    def test_unknown_keyword_is_a_config_error(self) -> None:
        with pytest.raises(tylertoo.ConfigError, match="unknown keyword"):
            tylertoo.serve(str(ROADS_PMTILES), bogus=1)  # type: ignore[call-arg]

    def test_repeated_name_is_a_config_error(self) -> None:
        with pytest.raises(tylertoo.ConfigError, match="two archives"):
            tylertoo.serve([ROADS_PMTILES, ROADS_PMTILES], port=free_port())

    def test_missing_archive_raises_before_serving(self, tmp_path: Path) -> None:
        with pytest.raises(RuntimeError, match="could not open"):
            tylertoo.serve(str(tmp_path / "missing.pmtiles"), port=free_port())

    def test_serves_the_archive_bytes(self) -> None:
        port = free_port()
        with serving([str(ROADS_PMTILES)], port=port):
            base = f"http://127.0.0.1:{port}"
            head = fetch(f"{base}/road-detections.pmtiles", {"Range": "bytes=0-126"})
            assert head == ROADS_PMTILES.read_bytes()[:127]
            assert b"maplibre-gl" in fetch(f"{base}/")

    def test_cors_is_opt_in(self) -> None:
        port = free_port()
        with serving(str(ROADS_PMTILES), port=port, cors="https://pmtiles.io"):
            url = f"http://127.0.0.1:{port}/index.json"
            fetch(url)
            for origin, allowed in [
                ("https://pmtiles.io", True),
                ("https://x.example", False),
            ]:
                req = urllib.request.Request(url, headers={"Origin": origin})
                with urllib.request.urlopen(req, timeout=5) as r:
                    assert (
                        r.headers["Access-Control-Allow-Origin"] == origin
                    ) is allowed

    # Windows has no SIGINT to send to a child; a console Ctrl-C reaches
    # the same `check_signals` poll.
    @pytest.mark.skipif(sys.platform == "win32", reason="no SIGINT on Windows")
    def test_ctrl_c_stops_the_server(self) -> None:
        port = free_port()
        with serving(str(ROADS_PMTILES), port=port) as proc:
            fetch(f"http://127.0.0.1:{port}/index.json")
            proc.send_signal(signal.SIGINT)
            _, err = proc.communicate(timeout=10)
        assert proc.returncode != 0
        assert "KeyboardInterrupt" in err
