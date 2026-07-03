#!/usr/bin/env python3
"""Byte-range HTTP server with per-request byte/request logging.

Serves files from a document root with HTTP Range support (206 Partial
Content) so DuckDB httpfs and the pmtiles range reader can fetch exactly
the byte ranges they need. Every request's response-body byte count is
accumulated so a client's total over-the-wire cost is measured exactly.

Control endpoints (never counted):
  GET /__reset  -> zero the counters, return 200
  GET /__stats  -> JSON {bytes, get_requests, head_requests, requests}

This is the single source of truth for `bytes_fetched` / `request_count`
in the access benchmark. The server holds no client-side cache, so cold
vs warm is entirely a property of the *client* process (a fresh DuckDB or
python process each run = cold cache).

Usage:
  uv run --with '' python logging_server.py <root_dir> <port>
"""
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else "."
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8899

_lock = threading.Lock()
_stats = {"bytes": 0, "get_requests": 0, "head_requests": 0}


def _reset():
    with _lock:
        _stats["bytes"] = 0
        _stats["get_requests"] = 0
        _stats["head_requests"] = 0


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence default stderr logging
        pass

    def _safe_path(self):
        rel = self.path.split("?", 1)[0].lstrip("/")
        full = os.path.abspath(os.path.join(ROOT, rel))
        if not full.startswith(ROOT):
            return None
        return full

    def do_GET(self):
        if self.path == "/__reset":
            _reset()
            self._json({"ok": True})
            return
        if self.path == "/__stats":
            with _lock:
                s = dict(_stats)
            s["requests"] = s["get_requests"] + s["head_requests"]
            self._json(s)
            return
        full = self._safe_path()
        if not full or not os.path.isfile(full):
            self.send_error(404)
            return
        size = os.path.getsize(full)
        rng = self.headers.get("Range")
        if rng and rng.startswith("bytes="):
            spec = rng[len("bytes="):].split(",")[0]
            a, _, b = spec.partition("-")
            if a == "":  # suffix range: last N bytes
                length = int(b)
                start = max(0, size - length)
                end = size - 1
            else:
                start = int(a)
                end = int(b) if b else size - 1
            end = min(end, size - 1)
            length = end - start + 1
            with open(full, "rb") as f:
                f.seek(start)
                body = f.read(length)
            self.send_response(206)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            with _lock:
                _stats["bytes"] += len(body)
                _stats["get_requests"] += 1
        else:
            with open(full, "rb") as f:
                body = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            with _lock:
                _stats["bytes"] += len(body)
                _stats["get_requests"] += 1

    def do_HEAD(self):
        full = self._safe_path()
        if not full or not os.path.isfile(full):
            self.send_error(404)
            return
        size = os.path.getsize(full)
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(size))
        self.end_headers()
        with _lock:
            _stats["head_requests"] += 1

    def _json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"serving {ROOT} on http://127.0.0.1:{PORT}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
