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
FIELDMAPS_PMTILES = (
    REPO / "tests" / "fixtures" / "golden" / "fieldmaps-boundaries.pmtiles"
)
REPEATS = 5


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port: int = s.getsockname()[1]
        return port


def free_ports(n: int) -> list[str]:
    """`n` distinct free ports, held open together so none repeats."""
    with contextlib.ExitStack() as stack:
        socks = [stack.enter_context(socket.socket()) for _ in range(n)]
        for s in socks:
            s.bind(("127.0.0.1", 0))
        return [str(s.getsockname()[1]) for s in socks]


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

    A child, not a thread: only the main thread can be stopped (Ctrl-C).
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


# Starts `tylertoo.serve` on a daemon thread per port, fetches from each,
# and lets the main thread end with the servers running. A server polls
# Python's signal handlers on each request and 200 ms after it, so fetches
# spaced across 200 ms spread the polls over the whole exit. pyarrow's
# library finalizers, which run after the interpreter's, make the exit as
# long as a real session's.
DAEMON_SERVERS = """
import sys, threading, time, urllib.request
import pyarrow, pyarrow.parquet, pyarrow.compute, tylertoo

archive, ports = sys.argv[1], [int(p) for p in sys.argv[2:]]
for port in ports:
    threading.Thread(
        target=tylertoo.serve, args=(archive,), kwargs={"port": port}, daemon=True
    ).start()
for port in ports:
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/index.json").read()
            break
        except OSError:
            time.sleep(0.05)
    else:
        sys.exit(f"port {port} never answered")
    time.sleep(0.2 / len(ports))
"""


# 60 copies of the archive overfill the socket buffers both ends.
PIPELINED_DOWNLOADS = """
import pathlib, socket
s = socket.create_connection(("127.0.0.1", ports[0]))
get = f"GET /{pathlib.Path(archive).stem}.pmtiles HTTP/1.1\\r\\nHost: x\\r\\n\\r\\n"
s.sendall(get.encode() * 60)
print(s.makefile("rb").readline().decode().strip())
"""


def run_child(code: str, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-c", code, *args],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )


class TestServeOnADaemonThread:
    def test_interpreter_exit_with_servers_running_is_clean(self) -> None:
        # Each exit raced the servers' poll of Python's signal handlers;
        # one that lost crashed the interpreter (SIGSEGV), in about 3 of 4
        # of these children. Repeated so that the race, if it were back,
        # would all but surely show.
        for _ in range(REPEATS):
            proc = run_child(DAEMON_SERVERS, str(ROADS_PMTILES), *free_ports(64))
            assert (proc.returncode, proc.stderr) == (0, "")

    def test_exit_while_a_response_is_being_sent_is_clean(self) -> None:
        # Pipelined whole-archive requests the child never reads: a
        # response is blocked mid-send and more wait behind it.
        code = DAEMON_SERVERS + PIPELINED_DOWNLOADS
        proc = run_child(code, str(FIELDMAPS_PMTILES), str(free_port()))
        assert (proc.returncode, proc.stderr) == (0, "")
        assert proc.stdout.endswith("HTTP/1.1 200 OK\n")
