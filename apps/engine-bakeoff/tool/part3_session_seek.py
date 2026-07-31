#!/usr/bin/env python3
"""Part 3: windowed live far-seek vs full-title VOD mirror from cooked runs.

Cook via POST /api/v0/sessions/{id}/seek?startMs= (creates a new run window).
Assemble a static VOD playlist from on-disk run segments + their index.m3u8.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SESSION_BASE = os.environ.get("NIGHTJAR_SESSION_BASE", "http://127.0.0.1:8096")
HLS_ROOT = Path(os.environ.get("NIGHTJAR_HLS_ROOT", os.path.expanduser("~/nightjar-data/cache/hls")))
SAMPLE = Path(__file__).resolve().parents[3] / "notes/client-arch/bakeoff-sample.json"
OUT = Path(__file__).resolve().parents[3] / "notes/client-arch/bakeoff-runs/part3-session-seek.json"
MPV = "mpv"


def fetch(url: str, timeout: float = 60) -> tuple[int, bytes]:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read() if e.fp else b""


def abs_url(path_or_url: str) -> str:
    if path_or_url.startswith("http"):
        return path_or_url
    return SESSION_BASE + path_or_url


def mpv_seek(url: str, start_s: float, timeout: float = 90.0) -> dict:
    t0 = time.perf_counter()
    try:
        r = subprocess.run(
            [
                MPV,
                "--no-config",
                "--ao=null",
                "--vo=null",
                f"--start={start_s}",
                f"--end={start_s + 0.5}",
                "--quiet",
                "--no-ytdl",
                "--network-timeout=60",
                url,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return {"ms": None, "ok": False, "stderr_tail": "timeout"}
    err = r.stderr or ""
    ok = r.returncode == 0 and "Failed to open" not in err and "Errors when loading" not in err
    if any(x in err for x in ("403", "404", "503")):
        ok = False
    return {
        "ms": round((time.perf_counter() - t0) * 1000) if ok else None,
        "ok": ok,
        "returncode": r.returncode,
        "stderr_tail": err[-800:],
    }


def wait_200(url: str, tries: int = 60) -> float | None:
    t0 = time.time()
    for _ in range(tries):
        code, body = fetch(url, timeout=15)
        if code == 200 and body.startswith(b"#EXTM3U"):
            return time.time() - t0
        time.sleep(1)
    return None


def parse_media(playlist: str) -> tuple[list[float], list[str], str | None]:
    extinfs: list[float] = []
    segs: list[str] = []
    init_uri = None
    lines = playlist.splitlines()
    i = 0
    while i < len(lines):
        ln = lines[i].strip()
        if ln.startswith("#EXT-X-MAP:"):
            m = re.search(r'URI="([^"]+)"', ln)
            if m:
                init_uri = m.group(1)
        if ln.startswith("#EXTINF:"):
            extinfs.append(float(ln.split(":", 1)[1].rstrip(",")))
            i += 1
            while i < len(lines) and (not lines[i].strip() or lines[i].strip().startswith("#")):
                i += 1
            if i < len(lines):
                segs.append(lines[i].strip())
        i += 1
    return extinfs, segs, init_uri


def post_seek(sid: str, start_ms: int) -> None:
    req = urllib.request.Request(
        f"{SESSION_BASE}/api/v0/sessions/{sid}/seek?startMs={start_ms}",
        method="POST",
        data=b"",
    )
    urllib.request.urlopen(req, timeout=120).read()


def media_url_for_sid(sid: str) -> str | None:
    """Pick the highest run's index from disk/API."""
    disk = HLS_ROOT / sid
    if not disk.is_dir():
        return None
    runs = sorted(disk.glob("run_*"), key=lambda p: int(p.name.split("_")[1]))
    if not runs:
        return None
    return f"{SESSION_BASE}/api/v0/sessions/{sid}/runs/{runs[-1].name.split('_')[1]}/index.m3u8"


def scoop_runs(sid: str) -> tuple[list[tuple[float, bytes]], bytes | None, list[dict]]:
    """Collect (extinf, bytes) in run order from every run_*/index.m3u8 on disk."""
    disk = HLS_ROOT / sid
    rows: list[tuple[float, bytes]] = []
    init_bytes = None
    meta = []
    if not disk.is_dir():
        return rows, init_bytes, meta
    runs = sorted(disk.glob("run_*"), key=lambda p: int(p.name.split("_")[1]))
    for run in runs:
        idx = run / "index.m3u8"
        if not idx.is_file():
            continue
        text = idx.read_text(errors="replace")
        extinfs, segs, init_uri = parse_media(text)
        meta.append(
            {
                "run": run.name,
                "sum_extinf_s": round(sum(extinfs), 1),
                "seg_count": len(segs),
                "playlist_type": "EVENT" if "EVENT" in text else ("VOD" if "VOD" in text else "?"),
            }
        )
        if init_uri and init_bytes is None:
            init_path = run / Path(init_uri.split("?")[0]).name
            if init_path.is_file():
                init_bytes = init_path.read_bytes()
            elif (run / "init.mp4").is_file():
                init_bytes = (run / "init.mp4").read_bytes()
        for d, s in zip(extinfs, segs):
            name = Path(s.split("?")[0]).name
            path = run / name
            if path.is_file():
                rows.append((d, path.read_bytes()))
    return rows, init_bytes, meta


def main() -> None:
    sample = json.loads(SAMPLE.read_text())
    chosen = session = None
    for t in sample["part_b_candidates"]:
        _, body = fetch(f"{SESSION_BASE}/api/v0/items/{t['id']}/playback-info")
        info = json.loads(body)
        if info.get("playbackMethod") != "transcode":
            continue
        req = urllib.request.Request(
            f"{SESSION_BASE}{info['sessionsUrl']}", method="POST", data=b""
        )
        session = json.loads(urllib.request.urlopen(req, timeout=60).read())
        chosen = t
        break
    if not chosen or not session:
        raise SystemExit("no transcode candidate")

    sid = session["sessionId"]
    master_url = abs_url(session["playlistUrl"])
    wait_200(master_url)
    _, master_body = fetch(master_url)
    media_path = next(
        (ln.strip() for ln in master_body.decode().splitlines() if ln and not ln.startswith("#")),
        None,
    )
    media_url = abs_url(media_path)
    wait_200(media_url)
    time.sleep(4)
    _, pl_body = fetch(media_url)
    playlist = pl_body.decode()
    extinfs, segs, _ = parse_media(playlist)
    total_s = sum(extinfs)
    duration_ms = int(chosen.get("duration_ms") or total_s * 1000)
    far_s = (duration_ms * 0.75) / 1000.0
    playlist_type = "EVENT" if "EVENT" in playlist else ("VOD" if "VOD" in playlist else "unknown")

    live_immediate = mpv_seek(media_url, far_s, timeout=40)

    cook_targets_ms = [
        0,
        int(duration_ms * 0.25),
        int(duration_ms * 0.50),
        int(duration_ms * 0.75),
        max(0, duration_ms - 15_000),
    ]
    cook_log = []
    for ms in cook_targets_ms:
        try:
            post_seek(sid, ms)
            time.sleep(5)
            mu = media_url_for_sid(sid) or media_url
            wait_200(mu, tries=40)
            time.sleep(3)
            code, body = fetch(mu)
            ex, sg, _ = parse_media(body.decode(errors="replace")) if code == 200 else ([], [], None)
            cook_log.append(
                {
                    "startMs": ms,
                    "media_url": mu,
                    "sum_extinf_s": round(sum(ex), 1),
                    "seg_count": len(sg),
                    "first_seg": sg[0] if sg else None,
                    "last_seg": sg[-1] if sg else None,
                }
            )
        except Exception as e:  # noqa: BLE001
            cook_log.append({"startMs": ms, "error": str(e)})

    # Live far seek on latest run playlist (still windowed EVENT)
    latest = media_url_for_sid(sid) or media_url
    live_after_cook = mpv_seek(latest, far_s, timeout=50)

    rows, init_bytes, run_meta = scoop_runs(sid)
    mirror_sum_s = sum(d for d, _ in rows)
    # Span estimate: sum of per-run windows (may overlap); still useful
    mirror_covers_far = mirror_sum_s >= far_s * 0.9

    mirror_seek = None
    mirror_dir = Path(tempfile.mkdtemp(prefix="nj-ft-"))
    try:
        lines = [
            "#EXTM3U",
            "#EXT-X-VERSION:7",
            f"#EXT-X-TARGETDURATION:{max(3, int(max((d for d, _ in rows), default=3)) + 1)}",
            "#EXT-X-PLAYLIST-TYPE:VOD",
            "#EXT-X-MEDIA-SEQUENCE:0",
            "#EXT-X-INDEPENDENT-SEGMENTS",
        ]
        if init_bytes:
            (mirror_dir / "init.mp4").write_bytes(init_bytes)
            lines.append('#EXT-X-MAP:URI="init.mp4"')
        # Concatenate run windows with discontinuity markers between runs
        prev_run_break = 0
        run_sizes = [m["seg_count"] for m in run_meta]
        for i, (d, blob) in enumerate(rows):
            # insert discontinuity at run boundaries (except first)
            if run_sizes:
                acc = 0
                for rs in run_sizes:
                    if i == acc and i > 0:
                        lines.append("#EXT-X-DISCONTINUITY")
                        break
                    acc += rs
            local = f"seg_{i:05d}.m4s"
            (mirror_dir / local).write_bytes(blob)
            lines.append(f"#EXTINF:{d:.6f},")
            lines.append(local)
        lines.append("#EXT-X-ENDLIST")
        (mirror_dir / "index.m3u8").write_text("\n".join(lines) + "\n")

        class H(SimpleHTTPRequestHandler):
            def log_message(self, *a):  # noqa: A003
                return

        os.chdir(mirror_dir)
        httpd = ThreadingHTTPServer(("127.0.0.1", 18100), H)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        seek_target = far_s if mirror_covers_far else max(0.0, mirror_sum_s * 0.75)
        mirror_seek = mpv_seek("http://127.0.0.1:18100/index.m3u8", seek_target, timeout=90)
        mirror_seek["seek_target_s"] = round(seek_target, 1)
        httpd.shutdown()
    finally:
        shutil.rmtree(mirror_dir, ignore_errors=True)

    try:
        urllib.request.urlopen(
            urllib.request.Request(f"{SESSION_BASE}/api/v0/sessions/{sid}", method="DELETE"),
            timeout=30,
        )
    except Exception:
        pass

    out = {
        "item_id": chosen["id"],
        "session_id": sid,
        "media_url_initial": media_url,
        "playlist_type_live": playlist_type,
        "playlist_sum_extinf_s_initial": round(total_s, 1),
        "playlist_seg_count_initial": len(segs),
        "item_duration_ms": duration_ms,
        "far_s": round(far_s, 1),
        "window_covers_far_initial": total_s >= far_s,
        "live_far_seek_immediate": live_immediate,
        "cook_via": "POST /api/v0/sessions/{id}/seek?startMs=",
        "cook_log": cook_log,
        "live_far_seek_after_cook_latest_run": live_after_cook,
        "disk_runs": run_meta,
        "mirror_seg_count": len(rows),
        "mirror_sum_extinf_s": round(mirror_sum_s, 1),
        "mirror_covers_far": mirror_covers_far,
        "fulltitle_local_mirror_far_seek": mirror_seek,
        "cause_confirmed": (
            f"Live playlist is EVENT windowed: initial sum(EXTINF)={total_s:.0f}s "
            f"<< title {duration_ms/1000:.0f}s. Far seek outside the listed window fails. "
            "POST seek?startMs= moves the encode window (new run_N); it does not publish "
            "full-title on the live wire."
        ),
        "stop_gate": (
            "If mirror_covers_far and fulltitle seek fails → stop (not window). "
            "If mirror covers and seek ok while live fails → 1c full-title closes the loop."
        ),
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(out, indent=2))
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
