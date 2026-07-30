#!/usr/bin/env python3
"""Spike C — Firefox + hls.js via geckodriver."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("SPIKE_BASE", "http://127.0.0.1:19641").rstrip("/")
MASTER = os.environ.get("SPIKE_MASTER", "master_nodisc.m3u8")
SPLICE_S = os.environ.get("SPIKE_SPLICE_S", "6")
OUT = os.environ.get("SPIKE_OUT", "/tmp/spike_c_firefox.json")
PORT = int(os.environ.get("SPIKE_GECKO_PORT", "19652"))
FF = os.environ.get(
    "FIREFOX_PATH", "/Applications/Firefox.app/Contents/MacOS/firefox"
)
PAGE = f"{BASE}/index.html?engine=hlsjs&master={MASTER}&spliceS={SPLICE_S}"
VARIANT = "nodisc" if "nodisc" in MASTER else "disc"


class Wd:
    def __init__(self, port: int):
        self.base = f"http://127.0.0.1:{port}"
        self.session = None

    def _req(self, method: str, path: str, body=None, timeout: float = 90):
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

    def execute_async(self, script: str):
        res = self._req(
            "POST",
            f"/session/{self.session}/execute/async",
            {"script": script, "args": []},
            timeout=120,
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
            raise SystemExit(f"geckodriver session failed: {last}")

        wd.get(PAGE)
        time.sleep(0.5)
        # execute_async: last arg is the callback.
        result = wd.execute_async(
            """
            var cb = arguments[arguments.length - 1];
            if (!window.__SPIKE) { cb({error: 'no __SPIKE'}); return; }
            window.__SPIKE.run().then(cb).catch(function (e) {
              cb({error: String(e)});
            });
            """
        )
        payload = {
            "consumer": "firefox_hlsjs",
            "variant": VARIANT,
            "master": MASTER,
            **(result or {}),
        }
        with open(OUT, "w") as f:
            json.dump(payload, f, indent=2)
            f.write("\n")
        print(
            f"firefox {VARIANT}: crossed={payload.get('crossed')} "
            f"uninterrupted={payload.get('uninterrupted')} "
            f"seekBack={payload.get('seekBackOk')}",
            file=sys.stderr,
        )
        return 0 if payload.get("crossed") else 2
    except Exception as e:
        with open(OUT, "w") as f:
            json.dump(
                {
                    "consumer": "firefox_hlsjs",
                    "variant": VARIANT,
                    "error": str(e),
                },
                f,
                indent=2,
            )
            f.write("\n")
        print(f"firefox error: {e}", file=sys.stderr)
        return 1
    finally:
        wd.delete()
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except Exception:
            proc.kill()


if __name__ == "__main__":
    raise SystemExit(main())
