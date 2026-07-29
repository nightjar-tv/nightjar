#!/usr/bin/env python3
"""Reproduce / verify Safari native HLS subtitle reassert after seek.

No manual Network-tab work: creates a session, curl-checks a subtitle
segment has real WebVTT, drives system Safari via safaridriver, polls
video.textTracks for 10s after a seek, prints PASS/FAIL + full trace.

  BASE=http://127.0.0.1:8098 ITEM=33 START_MS=120000 SEEK_TO_MS=300000 \
    REASSERT=none|teardown python3 scripts/safari_native_subs_seek.py

REASSERT modes (injected into the harness page):
  none      — leave DEFAULT=YES alone after seek (baseline fail)
  teardown  — disable until cues drop, then showing; wait for a cue
              whose time range covers currentTime (event-based)
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

BASE = os.environ.get("BASE", "http://127.0.0.1:8098").rstrip("/")
ITEM = os.environ.get("ITEM", "33")
START_MS = int(os.environ.get("START_MS", "120000"))
SEEK_TO_MS = int(os.environ.get("SEEK_TO_MS", "300000"))
REASSERT = os.environ.get("REASSERT", "none")
PORT = int(os.environ.get("SAFARIDRIVER_PORT", "4455"))
HARNESS_PORT = int(os.environ.get("HARNESS_PORT", "8765"))
POLL_MS = int(os.environ.get("POLL_MS", "250"))
AFTER_SEEK_S = float(os.environ.get("AFTER_SEEK_S", "10"))
LINEAR_WAIT_S = float(os.environ.get("LINEAR_WAIT_S", "30"))


def http_json(method: str, url: str, body=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"content-type": "application/json", "accept": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read().decode() or "{}"
        return r.status, json.loads(raw) if raw.strip() else {}


def http_text(url: str, timeout=60):
    req = urllib.request.Request(url, method="GET")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, r.headers.get("content-type", ""), r.read()


def wait_ok(url: str, timeout_s: float = 120.0) -> bool:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            status, _, body = http_text(url, timeout=5)
            if status == 200 and body:
                return True
        except Exception:
            pass
        time.sleep(0.25)
    return False


HARNESS_HTML = r"""<!doctype html>
<html>
<head><meta charset="utf-8"><title>nj safari subs seek</title></head>
<body>
<video id="v" controls playsinline muted></video>
<script>
window.__nj = { phase: 'boot', samples: [], error: null };
const PLAYLIST = __PLAYLIST__;
const START_S = __START_S__;
const SEEK_S = __SEEK_S__;
const REASSERT = __REASSERT__;
const POLL_MS = __POLL_MS__;

function snap(label) {
  const v = document.getElementById('v');
  const list = v.textTracks;
  const tracks = [];
  for (let i = 0; i < list.length; i++) {
    const t = list[i];
    const cues = t.cues;
    const active = t.activeCues;
    let cover = false;
    let activeStarts = [];
    if (active) {
      for (let j = 0; j < active.length; j++) {
        const c = active[j];
        activeStarts.push(+c.startTime.toFixed(3));
        if (c.startTime <= v.currentTime && v.currentTime < c.endTime) cover = true;
      }
    }
    tracks.push({
      i,
      label: t.label || t.language || t.kind,
      mode: t.mode,
      cues: cues ? cues.length : null,
      active: active ? active.length : null,
      cover,
      activeStarts
    });
  }
  const sample = {
    t: Math.round(performance.now()),
    label,
    currentTime: +v.currentTime.toFixed(3),
    seeking: v.seeking,
    readyState: v.readyState,
    tracks
  };
  window.__nj.samples.push(sample);
  return sample;
}

function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

async function waitCues(timeoutMs) {
  const v = document.getElementById('v');
  const t0 = performance.now();
  while (performance.now() - t0 < timeoutMs) {
    for (let i = 0; i < v.textTracks.length; i++) {
      const c = v.textTracks[i].cues;
      if (c && c.length > 0) return true;
    }
    await sleep(200);
  }
  return false;
}

function coveringActive(v) {
  for (let i = 0; i < v.textTracks.length; i++) {
    const active = v.textTracks[i].activeCues;
    if (!active) continue;
    for (let j = 0; j < active.length; j++) {
      const c = active[j];
      if (c.startTime <= v.currentTime && v.currentTime < c.endTime) return true;
    }
  }
  return false;
}

function waitCuesNull(track, timeoutMs) {
  return new Promise((resolve) => {
    const t0 = performance.now();
    const tick = () => {
      if (!track.cues) return resolve(true);
      if (performance.now() - t0 >= timeoutMs) return resolve(false);
      setTimeout(tick, 50);
    };
    tick();
  });
}

function waitCoveringCue(track, video, timeoutMs) {
  return new Promise((resolve) => {
    let done = false;
    const finish = (ok) => {
      if (done) return;
      done = true;
      track.removeEventListener('cuechange', onCue);
      resolve(ok);
    };
    const onCue = () => {
      if (coveringActive(video)) finish(true);
    };
    track.addEventListener('cuechange', onCue);
    if (coveringActive(video)) finish(true);
    setTimeout(() => finish(coveringActive(video)), timeoutMs);
  });
}

async function reassertTeardown(v) {
  // Full reset: disable every track and wait until WebKit drops the cue
  // list (cues === null), then re-show the wanted track and wait for a
  // cue whose range covers currentTime. Timer only as a hard ceiling.
  const wanted = 0;
  snap('reassert:disable');
  for (let i = 0; i < v.textTracks.length; i++) {
    v.textTracks[i].mode = 'disabled';
  }
  const track = v.textTracks[wanted];
  const dropped = await waitCuesNull(track, 5000);
  snap(dropped ? 'reassert:cues-dropped' : 'reassert:cues-drop-timeout');
  track.mode = 'showing';
  snap('reassert:showing');
  const covered = await waitCoveringCue(track, v, 10000);
  snap(covered ? 'reassert:cover-ok' : 'reassert:cover-timeout');
  return covered;
}

async function run() {
  const v = document.getElementById('v');
  window.__nj.phase = 'attach';
  v.muted = true;
  v.playsInline = true;
  // WebDriver Safari often exposes EXT-X-MEDIA tracks as showing but never
  // fetches the subtitle playlist until mode is assigned from script.
  v.textTracks.addEventListener('addtrack', (ev) => {
    const tr = ev.track;
    snap('addtrack');
    tr.mode = 'disabled';
    setTimeout(() => {
      tr.mode = 'showing';
      snap('addtrack-showing');
    }, 0);
  });
  v.src = START_S > 0 ? (PLAYLIST + '#t=' + START_S) : PLAYLIST;
  await new Promise((resolve, reject) => {
    v.addEventListener('loadedmetadata', () => {
      if (START_S > 0 && Math.abs(v.currentTime - START_S) > 1) v.currentTime = START_S;
      snap('loadedmetadata');
      resolve();
    }, { once: true });
    v.addEventListener('error', () => {
      reject(new Error('video error code=' + (v.error && v.error.code)));
    }, { once: true });
    setTimeout(() => reject(new Error('loadedmetadata timeout')), 60000);
  });
  // Wait for #t= / currentTime land to finish seeking before play.
  await new Promise((resolve) => {
    const t0 = performance.now();
    const tick = () => {
      if (!v.seeking) return resolve();
      if (performance.now() - t0 > 20000) return resolve();
      setTimeout(tick, 50);
    };
    tick();
  });
  snap('landed');
  // Safari WebDriver: play() can hang forever under autoplay policy.
  try {
    await Promise.race([v.play(), sleep(3000)]);
  } catch (e) {}
  snap('playing');
  window.__nj.phase = 'linear-wait';
  const linearOk = await waitCues(__LINEAR_WAIT_MS__);
  snap(linearOk ? 'linear:cues-ok' : 'linear:cues-fail');
  if (!linearOk) {
    window.__nj.phase = 'fail-linear';
    window.__nj.error = 'no cues during linear play rs=' + v.readyState + ' ns=' + v.networkState;
    return;
  }
  window.__nj.phase = 'seek';
  const target = SEEK_S;
  v.currentTime = target;
  await new Promise((resolve) => {
    const t0 = performance.now();
    const tick = () => {
      if (!v.seeking && Math.abs(v.currentTime - target) < 1.0) return resolve();
      if (performance.now() - t0 > 20000) return resolve();
      setTimeout(tick, 50);
    };
    v.addEventListener('seeked', () => resolve(), { once: true });
    tick();
  });
  snap('seeked');
  if (REASSERT === 'teardown') {
    window.__nj.phase = 'reassert';
    await reassertTeardown(v);
  } else {
    snap('reassert:none');
  }
  window.__nj.phase = 'poll';
  const pollUntil = performance.now() + __AFTER_SEEK_MS__;
  while (performance.now() < pollUntil) {
    snap('poll');
    if (coveringActive(v)) {
      snap('pass:cover');
      window.__nj.phase = 'pass';
      return;
    }
    await sleep(POLL_MS);
  }
  window.__nj.phase = 'fail-seek';
  window.__nj.error = 'no covering active cue within poll window';
  snap('fail');
}
run().catch((e) => {
  window.__nj.phase = 'error';
  window.__nj.error = String(e && e.message ? e.message : e);
  snap('error');
});
</script>
</body>
</html>
"""


class HarnessHandler(BaseHTTPRequestHandler):
    html = ""
    api_base = ""

    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path.startswith("/play"):
            body = self.html.encode()
            self.send_response(200)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path.startswith("/api/"):
            url = self.api_base + self.path
            try:
                req = urllib.request.Request(url, method="GET")
                with urllib.request.urlopen(req, timeout=60) as r:
                    data = r.read()
                    ctype = r.headers.get("content-type", "application/octet-stream")
                    self.send_response(r.status)
                    self.send_header("content-type", ctype)
                    self.send_header("cache-control", "no-store")
                    self.send_header("content-length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
            except urllib.error.HTTPError as e:
                body = e.read()
                self.send_response(e.code)
                self.send_header("content-type", e.headers.get("content-type", "text/plain"))
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except Exception as e:
                msg = str(e).encode()
                self.send_response(502)
                self.send_header("content-type", "text/plain")
                self.send_header("content-length", str(len(msg)))
                self.end_headers()
                self.wfile.write(msg)
            return
        self.send_response(404)
        self.end_headers()


class Wd:
    def __init__(self, port: int):
        self.base = f"http://127.0.0.1:{port}"
        self.session = None

    def _req(self, method: str, path: str, body=None, timeout: float = 30):
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
        try:
            res = self._req(
                "POST",
                "/session",
                {
                    "capabilities": {
                        "alwaysMatch": {
                            "browserName": "safari",
                            "webkit:alwaysAllowAutoplay": True,
                        }
                    }
                },
            )
        except urllib.error.HTTPError as e:
            body = e.read().decode()
            if "Allow remote automation" in body or "remote automation" in body:
                raise RuntimeError(
                    "Safari WebDriver blocked: enable Develop → Allow Remote "
                    "Automation in Safari Settings, then re-run. "
                    "(Playwright WebKit cannot exercise native HLS TEXT.)"
                ) from e
            raise RuntimeError(f"safaridriver session create failed: {body}") from e
        if "sessionId" not in (res.get("value") or {}):
            msg = (res.get("value") or {}).get("message") or res
            if "remote automation" in str(msg).lower():
                raise RuntimeError(
                    "Safari WebDriver blocked: enable Develop → Allow Remote "
                    "Automation in Safari Settings, then re-run."
                )
            raise RuntimeError(f"safaridriver session create failed: {msg}")
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


def find_sub_uri(master_text: str) -> str | None:
    for line in master_text.splitlines():
        if "TYPE=SUBTITLES" in line and "URI=" in line:
            m = re.search(r'URI="([^"]+)"', line)
            if m:
                return m.group(1)
    return None


def main() -> int:
    print(
        f"config base={BASE} item={ITEM} startMs={START_MS} "
        f"seekToMs={SEEK_TO_MS} reassert={REASSERT}"
    )

    # 1) Session
    post = f"{BASE}/api/v0/items/{ITEM}/sessions?startMs={START_MS}"
    try:
        status, started = http_json("POST", post)
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        print(f"FAIL session POST {e.code}: {body}")
        return 2
    if status not in (200, 201, 202):
        print(f"FAIL session POST status={status} body={started}")
        return 2
    session_id = started["sessionId"]
    playlist_path = started["playlistUrl"]
    master_url = f"{BASE}{playlist_path}"
    print(f"session {session_id} master={master_url}")

    if not wait_ok(master_url, 180):
        print("FAIL master never ready")
        return 2
    status, ctype, master_body = http_text(master_url)
    master_text = master_body.decode("utf-8", "replace")
    sub_rel = find_sub_uri(master_text)
    if not sub_rel:
        print("FAIL master has no SUBTITLES MEDIA")
        print(master_text[:500])
        return 2
    # Resolve relative to master directory
    master_dir = master_url.rsplit("/", 1)[0]
    sub_playlist = f"{master_dir}/{sub_rel}"
    print(f"sub playlist {sub_playlist}")
    # Touch index so the idle reaper does not reap before Safari attaches.
    wait_ok(f"{master_dir}/index.m3u8", 30)
    if not wait_ok(sub_playlist, 60):
        print("FAIL subtitle playlist never ready")
        return 2
    try:
        _, _, sub_pl = http_text(sub_playlist)
    except urllib.error.HTTPError as e:
        print(f"FAIL subtitle playlist fetch {e.code}")
        return 2
    sub_pl_text = sub_pl.decode("utf-8", "replace")
    # Playlist URIs look like `e2/seg060.vtt` (relative to .../subs/).
    seg_idx = START_MS // 2000
    want = f"seg{seg_idx:03d}.vtt"
    seg_rel = None
    for line in sub_pl_text.splitlines():
        if line.endswith(want):
            seg_rel = line.strip()
            break
    if not seg_rel:
        m = re.search(r"(\S+seg\d+\.vtt)", sub_pl_text)
        if not m:
            print("FAIL no seg in subtitle playlist")
            print(sub_pl_text[:400])
            return 2
        seg_rel = m.group(1)
    subs_dir = sub_playlist.rsplit("/", 1)[0]
    seg_url = f"{subs_dir}/{seg_rel}"
    print(f"curl seg {seg_url}")
    if not wait_ok(seg_url, 60):
        print("FAIL subtitle segment never ready")
        return 2
    try:
        st, ct, seg = http_text(seg_url)
    except urllib.error.HTTPError as e:
        print(f"FAIL subtitle segment fetch {e.code}")
        return 2
    seg_text = seg.decode("utf-8", "replace")
    print(f"curl seg status={st} content-type={ct} bytes={len(seg)}")
    if st != 200 or "WEBVTT" not in seg_text or "-->" not in seg_text:
        print("FAIL subtitle segment empty or not WebVTT")
        print(seg_text[:300])
        return 2
    print(f"curl seg ok preview={seg_text.splitlines()[:6]}")
    try:
        http_text(f"{master_dir}/seg{seg_idx:03d}.m4s", timeout=5)
    except Exception:
        pass

    # 2) Harness page — same-origin proxy so Safari loads master/index/segs/subs
    # through the harness host (relative URIs in playlists stay on that host).
    proxy_master = f"http://127.0.0.1:{HARNESS_PORT}{playlist_path}"
    html = (
        HARNESS_HTML.replace("__PLAYLIST__", json.dumps(proxy_master))
        .replace("__START_S__", str(START_MS / 1000.0))
        .replace("__SEEK_S__", str(SEEK_TO_MS / 1000.0))
        .replace("__REASSERT__", json.dumps(REASSERT))
        .replace("__POLL_MS__", str(POLL_MS))
        .replace("__LINEAR_WAIT_MS__", str(int(LINEAR_WAIT_S * 1000)))
        .replace("__AFTER_SEEK_MS__", str(int(AFTER_SEEK_S * 1000)))
    )
    HarnessHandler.html = html
    HarnessHandler.api_base = BASE
    httpd = HTTPServer(("127.0.0.1", HARNESS_PORT), HarnessHandler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    harness_url = f"http://127.0.0.1:{HARNESS_PORT}/play"

    # 3) Safari
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
            except RuntimeError as e:
                # Permanent config errors (e.g. Allow Remote Automation off).
                if "Allow Remote" in str(e) or "remote automation" in str(e).lower():
                    raise
                last_err = e
                time.sleep(0.25)
            except Exception as e:
                last_err = e
                time.sleep(0.25)
        else:
            err = proc.stderr.read().decode() if proc.stderr else ""
            raise RuntimeError(f"safaridriver never accepted sessions: {last_err}; {err}")

        wd.get(harness_url)
        # Wait for harness to finish
        deadline = time.time() + LINEAR_WAIT_S + AFTER_SEEK_S + 90
        state = None
        while time.time() < deadline:
            state = wd.execute("return window.__nj;")
            phase = (state or {}).get("phase")
            if phase in ("pass", "fail-seek", "fail-linear", "error"):
                break
            time.sleep(0.5)
        else:
            state = wd.execute("return window.__nj;")
            print("FAIL harness timeout")
            print(json.dumps(state, indent=2))
            return 1

        samples = state.get("samples") or []
        print("--- TRACE ---")
        for s in samples:
            tracks = s.get("tracks") or []
            tsummary = " | ".join(
                f"#{t['i']} {t['label']} mode={t['mode']} cues={t['cues']} "
                f"active={t['active']} cover={t['cover']}"
                for t in tracks
            ) or "no tracks"
            print(
                f"t={s['t']:>6} {s['label']:<24} "
                f"ct={s['currentTime']:<8} rs={s['readyState']} "
                f"seeking={s['seeking']} [{tsummary}]"
            )
        print("--- END TRACE ---")
        phase = state.get("phase")
        err = state.get("error")
        passed = phase == "pass"
        print(f"RESULT {'PASS' if passed else 'FAIL'} phase={phase} error={err} reassert={REASSERT}")
        return 0 if passed else 1
    finally:
        wd.delete()
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except Exception:
            proc.kill()
        httpd.shutdown()
        try:
            urllib.request.urlopen(
                urllib.request.Request(
                    f"{BASE}/api/v0/sessions/{session_id}", method="DELETE"
                ),
                timeout=10,
            ).read()
        except Exception:
            pass


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:
        print(f"FAIL exception: {e}")
        sys.exit(2)
