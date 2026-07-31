#!/usr/bin/env python3
"""Serve dogfood media by item id with HTTP Range (no BROWSER_V0 gate).

Also logs request patterns to /tmp/bakeoff-request-pattern.jsonl so Part A
attach confound (many small Ranges vs large sequential) is recorded without
a buffering reverse proxy (which cannot hold multi-GB bodies in memory).

Usage:
  python3 apps/engine-bakeoff/tool/dp_byte_serve.py
  NIGHTJAR_BASE=http://127.0.0.1:18097
"""

from __future__ import annotations

import json
import re
import sqlite3
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

DB = Path.home() / "nightjar-data" / "nightjar.db"
LISTEN = ("127.0.0.1", 18097)
LOG = Path("/tmp/bakeoff-request-pattern.jsonl")
RANGE_RE = re.compile(r"bytes=(\d*)-(\d*)")
_lock = threading.Lock()


def path_for(item_id: int) -> Path | None:
    con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    try:
        row = con.execute("SELECT path FROM media_items WHERE id = ?", (item_id,)).fetchone()
        return Path(row[0]) if row else None
    finally:
        con.close()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args) -> None:  # noqa: A003
        return

    def do_HEAD(self) -> None:  # noqa: N802
        self._serve(head_only=True)

    def do_GET(self) -> None:  # noqa: N802
        self._serve(head_only=False)

    def _serve(self, head_only: bool) -> None:
        t0 = time.perf_counter()
        m = re.match(r"^/items/(\d+)/stream$", self.path.split("?", 1)[0])
        if not m:
            self.send_error(404)
            return
        item_id = int(m.group(1))
        path = path_for(item_id)
        if path is None or not path.is_file():
            self.send_error(404, f"missing {item_id}")
            return
        size = path.stat().st_size
        range_hdr = self.headers.get("Range")
        start, end = 0, size - 1
        status = 200
        if range_hdr:
            rm = RANGE_RE.match(range_hdr.strip())
            if not rm:
                self.send_error(416)
                return
            a, b = rm.group(1), rm.group(2)
            start = int(a) if a else 0
            end = int(b) if b else size - 1
            if start > end or start >= size:
                self.send_error(416)
                return
            end = min(end, size - 1)
            status = 206
        length = end - start + 1
        self.send_response(status)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        self.send_header("Content-Type", "application/octet-stream")
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        sent = 0
        if not head_only:
            try:
                with path.open("rb") as f:
                    f.seek(start)
                    remaining = length
                    while remaining > 0:
                        chunk = f.read(min(1024 * 1024, remaining))
                        if not chunk:
                            break
                        self.wfile.write(chunk)
                        remaining -= len(chunk)
                        sent += len(chunk)
            except (BrokenPipeError, ConnectionResetError):
                pass
        row = {
            "t": time.time(),
            "method": self.command,
            "path": self.path.split("?")[0],
            "item_id": item_id,
            "range": range_hdr,
            "status": status,
            "bytes": sent if not head_only else 0,
            "elapsed_s": round(time.perf_counter() - t0, 4),
            "user_agent": self.headers.get("User-Agent"),
        }
        with _lock:
            with LOG.open("a") as f:
                f.write(json.dumps(row) + "\n")


def main() -> None:
    LOG.write_text("")
    httpd = ThreadingHTTPServer(LISTEN, Handler)
    print(f"dp byte serve on {LISTEN[0]}:{LISTEN[1]} db={DB} log={LOG}", flush=True)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
