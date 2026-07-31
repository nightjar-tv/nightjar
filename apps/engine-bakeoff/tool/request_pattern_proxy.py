#!/usr/bin/env python3
"""Reverse proxy that logs Range / method patterns for Part A attach confound.

Sits in front of Nightjar (:8096) on :18096. Engines point at the proxy.

Usage:
  python3 apps/engine-bakeoff/tool/request_pattern_proxy.py
  BASE=http://127.0.0.1:18096  (in the bake-off app)
"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.request import Request, urlopen

UPSTREAM = "http://127.0.0.1:18097"
LISTEN = ("127.0.0.1", 18096)
LOG = Path("/tmp/bakeoff-request-pattern.jsonl")
_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args) -> None:  # noqa: A003
        return

    def _proxy(self) -> None:
        t0 = time.perf_counter()
        body = self.rfile.read(int(self.headers.get("Content-Length", 0) or 0))
        url = UPSTREAM + self.path
        headers = {k: v for k, v in self.headers.items() if k.lower() not in {"host", "connection"}}
        req = Request(url, data=body if body else None, headers=headers, method=self.command)
        try:
            with urlopen(req, timeout=120) as resp:
                data = resp.read()
                status = resp.status
                out_headers = dict(resp.headers.items())
        except Exception as e:  # noqa: BLE001
            self.send_error(502, str(e))
            self._log(status=502, bytes_out=0, elapsed=time.perf_counter() - t0, err=str(e))
            return
        self.send_response(status)
        for k, v in out_headers.items():
            if k.lower() in {"transfer-encoding", "connection"}:
                continue
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)
        self._log(
            status=status,
            bytes_out=len(data),
            elapsed=time.perf_counter() - t0,
            err=None,
        )

    def _log(self, status: int, bytes_out: int, elapsed: float, err: str | None) -> None:
        row = {
            "t": time.time(),
            "method": self.command,
            "path": self.path.split("?")[0],
            "range": self.headers.get("Range"),
            "status": status,
            "bytes": bytes_out,
            "elapsed_s": round(elapsed, 4),
            "user_agent": self.headers.get("User-Agent"),
            "err": err,
        }
        with _lock:
            with LOG.open("a") as f:
                f.write(json.dumps(row) + "\n")

    def do_GET(self) -> None:  # noqa: N802
        self._proxy()

    def do_HEAD(self) -> None:  # noqa: N802
        self._proxy()

    def do_POST(self) -> None:  # noqa: N802
        self._proxy()

    def do_DELETE(self) -> None:  # noqa: N802
        self._proxy()


def main() -> None:
    LOG.write_text("")
    httpd = ThreadingHTTPServer(LISTEN, Handler)
    print(f"proxy {LISTEN[0]}:{LISTEN[1]} -> {UPSTREAM} log={LOG}", flush=True)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
