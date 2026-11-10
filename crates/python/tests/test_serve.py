"""Tests for `tylertoo.serve()`: the `tylertoo serve` server (#825)."""

import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.request
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


class TestServe:
    def test_serve_is_exported(self) -> None:
        assert "serve" in tylertoo.__all__

    def test_unknown_keyword_is_a_config_error(self) -> None:
        with pytest.raises(tylertoo.ConfigError, match="unknown keyword"):
            tylertoo.serve(str(ROADS_PMTILES), bogus=1)  # type: ignore[call-arg]

    def test_missing_archive_raises_before_serving(self, tmp_path: Path) -> None:
        with pytest.raises(RuntimeError, match="could not open"):
            tylertoo.serve(str(tmp_path / "missing.pmtiles"), port=free_port())

    def test_serves_the_archive_bytes(self) -> None:
        port = free_port()
        threading.Thread(
            target=tylertoo.serve,
            args=([ROADS_PMTILES],),
            kwargs={"port": port},
            daemon=True,
        ).start()
        base = f"http://127.0.0.1:{port}"
        head = fetch(f"{base}/road-detections.pmtiles", {"Range": "bytes=0-126"})
        assert head == ROADS_PMTILES.read_bytes()[:127]
        assert b"maplibre-gl" in fetch(f"{base}/")

    def test_ctrl_c_stops_the_server(self) -> None:
        port = free_port()
        proc = subprocess.Popen(
            [
                sys.executable,
                "-c",
                f"import tylertoo; tylertoo.serve({str(ROADS_PMTILES)!r}, port={port})",
            ],
            stderr=subprocess.PIPE,
            text=True,
        )
        fetch(f"http://127.0.0.1:{port}/index.json")
        proc.send_signal(signal.SIGINT)
        _, err = proc.communicate(timeout=10)
        assert proc.returncode != 0
        assert "KeyboardInterrupt" in err
