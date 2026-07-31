#!/usr/bin/env python3
"""Static HLS server for playlist-shape probe. No Nightjar server code."""
from __future__ import annotations

import argparse
import json
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parent / "static"


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)

    def end_headers(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_OPTIONS(self):
        self.send_response(204)
        self.end_headers()

    def do_POST(self):
        path = urlparse(self.path).path
        # Shape A: rewrite the mutable EVENT playlist to region B (or back to A).
        if path == "/mutate":
            length = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(length).decode() if length else "{}"
            try:
                want = json.loads(body).get("region", "b")
            except json.JSONDecodeError:
                want = "b"
            src = ROOT / (
                "shape_a_event_region_b.m3u8" if want == "b" else "shape_a_event_region_a_src.m3u8"
            )
            # region A source kept as shape_b_land_a copy for restore
            if want == "a":
                src = ROOT / "shape_b_land_a.m3u8"
            elif want == "b":
                src = ROOT / "shape_a_event_region_b.m3u8"
            dst = ROOT / "shape_a_event.m3u8"
            dst.write_text(src.read_text())
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"ok": True, "region": want, "bytes": dst.stat().st_size}).encode())
            return
        self.send_error(404)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8765)
    args = ap.parse_args()
    # Seed mutable playlist from land A
    (ROOT / "shape_a_event.m3u8").write_text((ROOT / "shape_b_land_a.m3u8").read_text())
    httpd = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"serving {ROOT} on http://127.0.0.1:{args.port}/")
    print("page: http://127.0.0.1:%d/page.html" % args.port)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
