#!/usr/bin/env python3
"""Part B mid-playback starve: compat-transcode session through relative throttle.

Configures byte rate at 50% of title bitrate_bps_est, records configured vs
achieved throughput over >=60s while mpv plays the session playlist.
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[3]
SAMPLE = ROOT / "notes" / "client-arch" / "bakeoff-sample.json"
OUT = ROOT / "notes" / "client-arch" / "bakeoff-runs" / "partb-starve.json"
SESSION_BASE = os.environ.get("NIGHTJAR_SESSION_BASE", "http://127.0.0.1:8096")
LISTEN = ("127.0.0.1", 18098)

_state = {
    "bps": 125000,
    "bytes_sent": 0,
    "t0": time.time(),
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args) -> None:  # noqa: A003
        return

    def do_GET(self) -> None:  # noqa: N802
        url = SESSION_BASE + self.path
        req = Request(url, headers={k: v for k, v in self.headers.items() if k.lower() != "host"})
        try:
            with urlopen(req, timeout=120) as resp:
                data = resp.read()
                status = resp.status
                headers = dict(resp.headers.items())
        except Exception as e:  # noqa: BLE001
            self.send_error(502, str(e))
            return
        # Throttle: sleep to match configured bps
        bps = max(1000, int(_state["bps"]))
        need = len(data) / bps
        time.sleep(need)
        _state["bytes_sent"] += len(data)
        self.send_response(status)
        for k, v in headers.items():
            if k.lower() in {"transfer-encoding", "connection", "content-length"}:
                continue
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def achieved_bps() -> float:
    elapsed = max(0.001, time.time() - _state["t0"])
    return _state["bytes_sent"] / elapsed


def main() -> None:
    sample = json.loads(SAMPLE.read_text())
    # Pick first candidate that is actually transcode
    chosen = None
    session = None
    for t in sample["part_b_candidates"]:
        info = json.loads(
            urllib.request.urlopen(
                f"{SESSION_BASE}/api/v0/items/{t['id']}/playback-info", timeout=30
            ).read()
        )
        if info.get("playbackMethod") != "transcode":
            continue
        req = urllib.request.Request(
            f"{SESSION_BASE}{info['sessionsUrl']}", method="POST", data=b""
        )
        with urllib.request.urlopen(req, timeout=60) as resp:
            session = json.loads(resp.read())
        chosen = t
        break
    if not chosen or not session:
        raise SystemExit("no compat-transcode candidate")

    bitrate = chosen.get("bitrate_bps_est") or 4_000_000
    configured = max(50_000, bitrate // 2)
    _state["bps"] = configured
    _state["bytes_sent"] = 0
    _state["t0"] = time.time()

    httpd = ThreadingHTTPServer(LISTEN, Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    time.sleep(0.3)

    playlist = f"http://{LISTEN[0]}:{LISTEN[1]}{session['playlistUrl']}"
    # Let session cook a bit on origin
    time.sleep(3)

    proc = subprocess.Popen(
        [
            "mpv",
            "--no-config",
            "--ao=null",
            "--vo=null",
            "--quiet",
            "--no-ytdl",
            "--network-timeout=60",
            playlist,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    samples = []
    for i in range(12):  # 60s
        time.sleep(5)
        samples.append(
            {
                "t": round(time.time() - _state["t0"], 1),
                "bytes_sent": _state["bytes_sent"],
                "achieved_bps": round(achieved_bps()),
                "configured_bps": configured,
            }
        )
        print(samples[-1], flush=True)

    proc.kill()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    httpd.shutdown()

    urllib.request.urlopen(
        urllib.request.Request(
            f"{SESSION_BASE}/api/v0/sessions/{session['sessionId']}", method="DELETE"
        ),
        timeout=30,
    )

    avg_achieved = sum(s["achieved_bps"] for s in samples) / max(1, len(samples))
    out = {
        "item_id": chosen["id"],
        "bitrate_bps_est": bitrate,
        "configured_bps": configured,
        "achieved_bps_avg": round(avg_achieved),
        "starved_ratio": round(avg_achieved / max(1, configured), 3),
        "note": (
            "compat-transcode playlist via throttle proxy; "
            "mpv stays on single rendition (Step 3). Observe stall vs recover over 60s."
        ),
        "samples": samples,
        "viewer": "expect stalls when segment fetch > EXTINF; no client downshift",
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(out, indent=2))
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
