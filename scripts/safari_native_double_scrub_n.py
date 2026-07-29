#!/usr/bin/env python3
"""Desktop Safari mid-playback double-scrub harness (n trials per engine).

Product default is hls.js on desktop Safari (ADR-0017). Native HLS is the
`?njNativeHls=1` hatch only. Default ENGINES runs both so results match
dual-engine dogfood (hls.js + native hatch), not native-only.

Each trial:
  attach → play a bit → scrub A → immediately scrub B (no wait for play) →
  score whether currentTime advances past B.

  BASE=http://127.0.0.1:8096 ITEM=33 N=7 GAP_MS=300 \
    ENGINES=hlsjs,native \
    python3 scripts/safari_native_double_scrub_n.py

ENGINES: comma list of `hlsjs` (default attach) and/or `native` (hatch).
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("BASE", "http://127.0.0.1:8096").rstrip("/")
ITEM = os.environ.get("ITEM", "33")
N = int(os.environ.get("N", "7"))
GAP_MS = int(os.environ.get("GAP_MS", "300"))
A_S = float(os.environ.get("A_S", "258"))
B_S = float(os.environ.get("B_S", "748"))
PORT = int(os.environ.get("SAFARIDRIVER_PORT", "4445"))
ENGINES_RAW = os.environ.get("ENGINES", "hlsjs,native")
OUT = os.environ.get("OUT", "/tmp/nj-desktop-double-scrub-n.jsonl")


def parse_engines(raw: str) -> list[str]:
    out: list[str] = []
    for part in raw.split(","):
        name = part.strip().lower()
        if not name:
            continue
        if name in ("hlsjs", "hls.js", "default"):
            name = "hlsjs"
        elif name in ("native", "native-hls", "nativehls"):
            name = "native"
        else:
            raise SystemExit(f"unknown ENGINES entry {part!r}; use hlsjs and/or native")
        if name not in out:
            out.append(name)
    if not out:
        raise SystemExit("ENGINES is empty")
    return out


def item_url(engine: str) -> str:
    base = f"{BASE}/items/{ITEM}"
    if engine == "native":
        return f"{base}?njNativeHls=1"
    return base


class Wd:
    def __init__(self, port: int):
        self.base = f"http://127.0.0.1:{port}"
        self.session = None

    def _req(self, method: str, path: str, body=None, timeout=120):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            self.base + path,
            data=data,
            method=method,
            headers={"content-type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read().decode() or "{}"
            return json.loads(raw)

    def start(self):
        res = self._req(
            "POST",
            "/session",
            {"capabilities": {"alwaysMatch": {"browserName": "safari"}}},
        )
        self.session = res["value"]["sessionId"]

    def delete(self):
        if self.session:
            try:
                self._req("DELETE", f"/session/{self.session}")
            except Exception:
                pass
            self.session = None

    def get(self, url: str):
        self._req("POST", f"/session/{self.session}/url", {"url": url})

    def exec_sync(self, script: str, args=None):
        body = {
            "script": script,
            "args": args or [],
        }
        res = self._req(
            "POST",
            f"/session/{self.session}/execute/sync",
            body,
            timeout=180,
        )
        return res.get("value")


PROBE_JS = """
var v = document.querySelector('video');
if (!v) return {ok:false, reason:'no video'};
return {
  ok: true,
  t: +v.currentTime.toFixed(3),
  dur: isFinite(v.duration) ? +v.duration.toFixed(2) : null,
  rs: v.readyState,
  paused: v.paused,
  seeking: v.seeking,
  ns: v.networkState
};
"""

PLAY_JS = """
var v = document.querySelector('video');
if (!v) return {ok:false, reason:'no video'};
v.muted = true;
try { v.play(); } catch (e) { return {ok:false, reason:String(e)}; }
return {ok:true};
"""

SEEK_JS = """
var t = arguments[0];
var v = document.querySelector('video');
if (!v) return {ok:false, reason:'no video'};
v.muted = true;
v.currentTime = t;
return {ok:true, t: v.currentTime};
"""


def wait_attach(wd: Wd, timeout_s: float = 90.0):
    deadline = time.time() + timeout_s
    wd.exec_sync(PLAY_JS)
    last = None
    while time.time() < deadline:
        p = wd.exec_sync(PROBE_JS)
        last = p
        if p and p.get("ok") and (p.get("dur") or 0) > 60 and (p.get("t") or 0) > 1.0:
            return {"ok": True, **p}
        if p and p.get("ok") and (p.get("dur") or 0) > 60 and p.get("paused"):
            wd.exec_sync(PLAY_JS)
        time.sleep(0.25)
    return {"ok": False, "reason": "attach timeout", "last": last}


def scrub_before_play(wd: Wd, a_s: float, b_s: float, gap_ms: int):
    before = wd.exec_sync(PROBE_JS)
    wd.exec_sync(SEEK_JS, [a_s])
    time.sleep(gap_ms / 1000.0)
    wd.exec_sync(SEEK_JS, [b_s])
    # Let land-commit quiet elapse after the final seek.
    time.sleep(max(0.4, gap_ms / 1000.0))
    land = b_s
    watch_start = time.time()
    max_t = 0.0
    while time.time() - watch_start < 45.0:
        p = wd.exec_sync(PROBE_JS) or {}
        t = float(p.get("t") or 0)
        if t > max_t:
            max_t = t
        if t >= land + 1.5:
            t1 = t
            time.sleep(1.5)
            p2 = wd.exec_sync(PROBE_JS) or {}
            t2 = float(p2.get("t") or 0)
            if t2 > t1 + 0.4:
                return {
                    "ok": True,
                    "stuck": False,
                    "tBefore": before.get("t") if before else None,
                    "a": a_s,
                    "b": b_s,
                    "finalT": t2,
                    "maxT": max(max_t, t2),
                    "waitMs": int((time.time() - watch_start) * 1000),
                    "paused": p2.get("paused"),
                    "seeking": p2.get("seeking"),
                }
        time.sleep(0.3)
    last = wd.exec_sync(PROBE_JS) or {}
    return {
        "ok": False,
        "stuck": True,
        "tBefore": before.get("t") if before else None,
        "a": a_s,
        "b": b_s,
        "finalT": last.get("t"),
        "maxT": max_t,
        "waitMs": int((time.time() - watch_start) * 1000),
        "paused": last.get("paused"),
        "seeking": last.get("seeking"),
        "rs": last.get("rs"),
        "ns": last.get("ns"),
    }


def run_engine(engine: str, out_path: str) -> dict:
    url = item_url(engine)
    print(
        f"# engine={engine} double scrub-before-play N={N} gap={GAP_MS}ms url={url}",
        flush=True,
    )

    passes = 0
    sticks = 0
    errors = 0

    for i in range(1, N + 1):
        wd = Wd(PORT)
        row = {
            "engine": engine,
            "trial": i,
            "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }
        try:
            wd.start()
            wd.get(url)
            time.sleep(2)
            attach = wait_attach(wd)
            row["attach"] = attach
            if not attach or not attach.get("ok"):
                row["result"] = "attach_fail"
                errors += 1
            else:
                scrub = scrub_before_play(wd, A_S, B_S, GAP_MS)
                row["scrub"] = scrub
                if scrub and scrub.get("ok"):
                    row["result"] = "pass"
                    passes += 1
                elif scrub and scrub.get("stuck"):
                    row["result"] = "stick"
                    sticks += 1
                else:
                    row["result"] = "scrub_fail"
                    errors += 1
        except Exception as e:
            row["result"] = "error"
            row["error"] = str(e)
            errors += 1
        finally:
            wd.delete()

        line = json.dumps(row)
        with open(out_path, "a") as f:
            f.write(line + "\n")
        print(line, flush=True)
        time.sleep(1.5)

    summary = {
        "type": "summary",
        "engine": engine,
        "n": N,
        "pass": passes,
        "stick": sticks,
        "error": errors,
        "gapMs": GAP_MS,
        "aS": A_S,
        "bS": B_S,
    }
    with open(out_path, "a") as f:
        f.write(json.dumps(summary) + "\n")
    print(json.dumps(summary), flush=True)
    return summary


def main() -> int:
    engines = parse_engines(ENGINES_RAW)

    try:
        urllib.request.urlopen(f"{BASE}/api/v0/items/{ITEM}", timeout=3)
    except Exception as e:
        print(f"# FATAL: nightjar not reachable at {BASE}: {e}", flush=True)
        return 2

    try:
        urllib.request.urlopen(f"http://127.0.0.1:{PORT}/status", timeout=2)
    except Exception:
        subprocess.Popen(
            ["safaridriver", "-p", str(PORT)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        time.sleep(1.5)

    open(OUT, "w").close()
    print(f"# engines={','.join(engines)} out={OUT}", flush=True)

    summaries = []
    for engine in engines:
        summaries.append(run_engine(engine, OUT))

    overall = {
        "type": "overall",
        "engines": engines,
        "summaries": summaries,
        "pass": all(s["stick"] == 0 and s["error"] == 0 for s in summaries),
    }
    with open(OUT, "a") as f:
        f.write(json.dumps(overall) + "\n")
    print(json.dumps(overall), flush=True)
    return 0 if overall["pass"] else 1


if __name__ == "__main__":
    sys.exit(main())
