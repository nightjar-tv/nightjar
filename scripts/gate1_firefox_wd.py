#!/usr/bin/env python3
"""Drive system Firefox via geckodriver (W3C WebDriver). Play + seek Gate 1 check."""
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
PORT = int(os.environ.get("GECKODRIVER_PORT", "4446"))
FF = os.environ.get(
    "FIREFOX_PATH", "/Applications/Firefox.app/Contents/MacOS/firefox"
)


class Wd:
    def __init__(self, port: int):
        self.base = f"http://127.0.0.1:{port}"
        self.session = None

    def _req(self, method: str, path: str, body=None, timeout: float = 60):
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
            {
                "capabilities": {
                    "alwaysMatch": {
                        "browserName": "firefox",
                        "moz:firefoxOptions": {
                            "binary": FF,
                            "args": ["-headless"],
                            "prefs": {
                                "media.autoplay.default": 0,
                                "media.autoplay.enabled.user-gestures-needed": False,
                            },
                        },
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

    def execute(self, script: str, args=None):
        res = self._req(
            "POST",
            f"/session/{self.session}/execute/sync",
            {"script": script, "args": args or []},
        )
        return res["value"]

    def execute_async(self, script: str, args=None):
        res = self._req(
            "POST",
            f"/session/{self.session}/execute/async",
            {"script": script, "args": args or []},
            timeout=90,
        )
        return res["value"]


def main() -> int:
    proc = subprocess.Popen(
        ["geckodriver", "--port", str(PORT)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(0.5)
    wd = Wd(PORT)
    try:
        last = None
        for _ in range(40):
            try:
                wd.start()
                break
            except Exception as e:
                last = e
                time.sleep(0.25)
        else:
            raise RuntimeError(f"geckodriver never accepted sessions: {last}")

        wd.get(URL)
        time.sleep(1.0)
        result = wd.execute_async(
            """
            const done = arguments[arguments.length - 1];
            const v = document.querySelector('video');
            if (!v) return done({ok:false, reason:'no video'});
            v.muted = true;
            v.play().then(() => {
              const t0 = Date.now();
              const waitPlay = () => {
                if (v.currentTime > 0.05 && v.readyState >= 2) {
                  const dur = v.duration;
                  if (!isFinite(dur) || dur < 2) return done({ok:false, reason:'bad duration '+dur});
                  const target = Math.min(dur * 0.5, dur - 0.5);
                  v.currentTime = target;
                  const t1 = Date.now();
                  const waitSeek = () => {
                    if (Math.abs(v.currentTime - target) < 0.75 && !v.seeking) {
                      return done({ok:true, play_ms: Date.now()-t0, seek_ms: Date.now()-t1, duration:+dur.toFixed(2), at:+v.currentTime.toFixed(2)});
                    }
                    if (Date.now() - t1 > 10000) return done({ok:false, reason:'seek stall at '+v.currentTime});
                    setTimeout(waitSeek, 50);
                  };
                  return waitSeek();
                }
                if (Date.now() - t0 > 10000) {
                  return done({ok:false, reason:'no progress rs='+v.readyState+' ns='+v.networkState+' err='+(v.error&&v.error.code)});
                }
                setTimeout(waitPlay, 50);
              };
              waitPlay();
            }).catch((e) => done({ok:false, reason:'play: '+e}));
            """
        )
        print(f"firefox: {json.dumps(result)}")
        return 0 if isinstance(result, dict) and result.get("ok") is True else 1
    except Exception as e:
        print(f"firefox: {json.dumps({'ok': False, 'reason': str(e)})}")
        return 1
    finally:
        try:
            wd.delete()
        except Exception:
            pass
        proc.kill()
        try:
            proc.wait(timeout=5)
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())
