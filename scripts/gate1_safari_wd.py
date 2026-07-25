#!/usr/bin/env python3
"""Drive system Safari via safaridriver (W3C WebDriver). Play + seek Gate 1 check."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("BASE_URL", "http://127.0.0.1:18122")
ITEM = os.environ.get("ITEM_ID", "1")
URL = f"{BASE}/items/{ITEM}"
PORT = int(os.environ.get("SAFARIDRIVER_PORT", "4445"))

PLAY_SEEK_JS = """
var v = document.querySelector('video');
if (!v) return {ok:false, reason:'no video'};
v.muted = true;
try { v.play(); } catch (e) { return {ok:false, reason:'play: '+e}; }
var start = Date.now();
while (true) {
  if (v.currentTime > 0.05 && v.readyState >= 2) break;
  if (Date.now() - start > 8000) {
    return {ok:false, reason:'no progress rs='+v.readyState+' ns='+v.networkState+' err='+(v.error && v.error.code)};
  }
}
var dur = v.duration;
if (!isFinite(dur) || dur < 2) return {ok:false, reason:'bad duration '+dur};
var target = Math.min(dur * 0.5, dur - 0.5);
v.currentTime = target;
start = Date.now();
while (true) {
  if (Math.abs(v.currentTime - target) < 0.75 && !v.seeking) break;
  if (Date.now() - start > 8000) {
    return {ok:false, reason:'seek stall at '+v.currentTime};
  }
}
return {ok:true, duration: +dur.toFixed(2), at: +v.currentTime.toFixed(2)};
"""


class Wd:
    def __init__(self, port: int):
        self.base = f"http://127.0.0.1:{port}"
        self.session = None

    def _req(self, method: str, path: str, body=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            self.base + path,
            data=data,
            method=method,
            headers={"content-type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=60) as r:
            raw = r.read().decode() or "{}"
            return json.loads(raw)

    def start(self):
        res = self._req(
            "POST",
            "/session",
            {
                "capabilities": {
                    "alwaysMatch": {
                        "browserName": "safari",
                    }
                }
            },
        )
        self.session = res["value"]["sessionId"]

    def delete(self):
        if self.session:
            try:
                self._req("DELETE", f"/session/{self.session}")
            except Exception:
                pass

    def get(self, url: str):
        self._req("POST", f"/session/{self.session}/url", {"url": url})

    def execute(self, script: str):
        res = self._req(
            "POST",
            f"/session/{self.session}/execute/sync",
            {"script": script, "args": []},
        )
        return res["value"]


def main() -> int:
    proc = subprocess.Popen(
        ["safaridriver", "--port", str(PORT)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    time.sleep(0.8)
    wd = Wd(PORT)
    try:
        last_err = None
        for _ in range(40):
            try:
                wd.start()
                break
            except Exception as e:
                last_err = e
                time.sleep(0.25)
        else:
            err = proc.stderr.read().decode() if proc.stderr else ""
            raise RuntimeError(f"safaridriver never accepted sessions: {last_err}; {err}")

        wd.get(URL)
        time.sleep(1.0)
        result = wd.execute(PLAY_SEEK_JS)
        print(f"safari: {json.dumps(result)}")
        return 0 if isinstance(result, dict) and result.get("ok") is True else 1
    except Exception as e:
        print(f"safari: {json.dumps({'ok': False, 'reason': str(e)})}")
        return 1
    finally:
        try:
            wd.delete()
        except Exception:
            pass
        proc.kill()
        try:
            proc.wait(timeout=3)
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())
