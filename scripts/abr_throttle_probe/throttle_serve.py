#!/usr/bin/env python3
"""Throttle + request log for ABR bake-off Step 3.

Serves scripts/abr_throttle_probe/static with a per-connection byte rate
limit. Appends every GET to access.jsonl so results survive server death.

Usage:
  python3 scripts/abr_throttle_probe/throttle_serve.py --bps 125000 --port 8765
"""

from __future__ import annotations

import argparse
import json
import threading
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

HERE = Path(__file__).resolve().parent
ROOT = HERE / "static"
ACCESS = HERE / "access.jsonl"
BPS = 125_000
LOCK = threading.Lock()


def append_log(row: dict) -> None:
    with LOCK:
        with ACCESS.open("a", encoding="utf-8") as f:
            f.write(json.dumps(row) + "\n")


class Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)

    def log_message(self, fmt: str, *args) -> None:
        pass

    def end_headers(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        super().end_headers()

    def do_OPTIONS(self):
        self.send_response(204)
        self.end_headers()

    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/reset":
            with LOCK:
                ACCESS.write_text("")
            self.send_response(204)
            self.end_headers()
            return
        if path == "/health":
            body = b"ok"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        fs_path = Path(self.translate_path(self.path))
        if not fs_path.is_file():
            self.send_error(404)
            return

        data = fs_path.read_bytes()
        t0 = time.time()
        self.send_response(200)
        self.send_header("Content-Type", self.guess_type(str(fs_path)))
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()

        sent = 0
        chunk = max(8 * 1024, max(BPS, 1) // 20)
        try:
            while sent < len(data):
                n = min(chunk, len(data) - sent)
                self.wfile.write(data[sent : sent + n])
                self.wfile.flush()
                sent += n
                if BPS > 0 and sent < len(data):
                    time.sleep(n / BPS)
        except (BrokenPipeError, ConnectionResetError):
            pass

        elapsed = time.time() - t0
        rendition = None
        for r in ("hi", "mid", "lo"):
            if f"/{r}/" in path:
                rendition = r
                break
        row = {
            "t": t0,
            "path": path,
            "bytes": len(data),
            "sent": sent,
            "elapsed_s": round(elapsed, 3),
            "rendition": rendition,
            "ua": (self.headers.get("User-Agent") or "")[:100],
        }
        append_log(row)
        print(
            f"{elapsed:6.3f}s {sent:7}/{len(data):7}B {rendition or '-':3} {path}",
            flush=True,
        )


def main() -> int:
    global BPS
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--bps", type=int, default=125_000)
    ap.add_argument("--bind", default="127.0.0.1")
    args = ap.parse_args()
    BPS = args.bps
    ACCESS.write_text("")
    httpd = ThreadingHTTPServer((args.bind, args.port), Handler)
    httpd.daemon_threads = True
    print(
        f"serving {ROOT} on http://{args.bind}:{args.port}/ "
        f"bps={BPS} ({BPS * 8 / 1e6:.2f} Mbps) log={ACCESS}",
        flush=True,
    )
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
