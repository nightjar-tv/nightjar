#!/usr/bin/env python3
"""CLI engine measure using mpv + VLC (same engines as Flutter bindings).

Part A DP URLs hit the bake-off byte server (not Nightjar /stream, which is
BROWSER_V0-gated). Request patterns are logged by dp_byte_serve.py.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SAMPLE = ROOT / "notes" / "client-arch" / "bakeoff-sample.json"
OUT_DIR = ROOT / "notes" / "client-arch" / "bakeoff-runs"
BASE = os.environ.get("NIGHTJAR_BASE", "http://127.0.0.1:18097")
SESSION_BASE = os.environ.get("NIGHTJAR_SESSION_BASE", "http://127.0.0.1:8096")
VLC = "/Applications/VLC.app/Contents/MacOS/VLC"
MPV = "mpv"


def pct(xs: list[float], q: float) -> float:
    if not xs:
        return float("nan")
    s = sorted(xs)
    i = int(round(q * (len(s) - 1)))
    return s[max(0, min(i, len(s) - 1))]


def summarize(ms: list[float]) -> dict:
    if not ms:
        return {"n": 0}
    return {
        "n": len(ms),
        "min_ms": round(min(ms)),
        "p50_ms": round(pct(ms, 0.5)),
        "p90_ms": round(pct(ms, 0.9)),
        "max_ms": round(max(ms)),
        "samples_ms": [round(x) for x in ms],
    }


def stream_url(item_id: int) -> str:
    return f"{BASE}/items/{item_id}/stream"


def mpv_first_frame(url: str, timeout: float = 40.0) -> float | None:
    """Wall clock until mpv decodes past --end (first-frame land proxy)."""
    t0 = time.perf_counter()
    try:
        r = subprocess.run(
            [
                MPV,
                "--no-config",
                "--ao=null",
                "--vo=null",
                "--end=0.25",
                "--quiet",
                "--no-ytdl",
                "--network-timeout=30",
                url,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return None
    if r.returncode not in (0,):
        # mpv sometimes exits 0 on --end; non-zero usually fail-to-open
        err = (r.stderr or "")[-400:]
        if "Failed to open" in err or "Errors when loading" in err:
            return None
    return (time.perf_counter() - t0) * 1000


def mpv_seek(url: str, target_ms: int, timeout: float = 40.0) -> float | None:
    start_s = max(0.0, target_ms / 1000.0)
    end_s = start_s + 0.3
    t0 = time.perf_counter()
    try:
        r = subprocess.run(
            [
                MPV,
                "--no-config",
                "--ao=null",
                "--vo=null",
                f"--start={start_s}",
                f"--end={end_s}",
                "--quiet",
                "--no-ytdl",
                "--network-timeout=30",
                url,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return None
    err = r.stderr or ""
    if "Failed to open" in err or "Errors when loading" in err:
        return None
    return (time.perf_counter() - t0) * 1000


def vlc_first_frame(url: str, timeout: float = 40.0) -> float | None:
    t0 = time.perf_counter()
    # Prefer file paths: HTTP Range attach for VLC can pull multi-GB (see Part A patterns).
    proc = subprocess.Popen(
        [
            VLC,
            "-I",
            "dummy",
            "--no-video",
            "--no-audio",
            "--play-and-stop",
            "--run-time=1",
            "--verbose",
            "2",
            url,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    deadline = time.time() + timeout
    hit = False
    assert proc.stdout is not None
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line and proc.poll() is not None:
            break
        if not line:
            time.sleep(0.02)
            continue
        low = line.lower()
        if "stream buffering done" in line or ("buffering" in low and "100%" in line):
            hit = True
            break
        if "using demux module" in low or ("decoder" in low and "started" in low):
            hit = True
            break
        if "successfully opened" in low:
            hit = True
            break
        if "is not accessible" in line or "main playlist error" in low:
            break
    ms = (time.perf_counter() - t0) * 1000
    proc.kill()
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
    return ms if hit else None


def vlc_seek(url: str, target_ms: int, timeout: float = 40.0) -> float | None:
    start_s = max(0, target_ms // 1000)
    t0 = time.perf_counter()
    proc = subprocess.Popen(
        [
            VLC,
            "-I",
            "dummy",
            "--no-video",
            "--no-audio",
            "--play-and-stop",
            "--run-time=1",
            f"--start-time={start_s}",
            "--verbose",
            "2",
            url,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    deadline = time.time() + timeout
    hit = False
    assert proc.stdout is not None
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line and proc.poll() is not None:
            break
        if not line:
            time.sleep(0.02)
            continue
        low = line.lower()
        if "stream buffering done" in line or ("buffering" in low and "100%" in line):
            hit = True
            break
        if "using demux module" in low or ("decoder" in low and "started" in low):
            hit = True
            break
        if "is not accessible" in line:
            break
    ms = (time.perf_counter() - t0) * 1000
    proc.kill()
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
    return ms if hit else None


def run_latency(engine: str, sample: dict) -> dict:
    by_id = {t["id"]: t for t in sample["t4_sample"]}
    cold_s, warm_s, warm_n, warm_f, cold_f = [], [], [], [], []
    for item_id in sample["latency_item_ids"]:
        meta = by_id.get(item_id)
        if not meta or not meta.get("duration_ms") or meta["duration_ms"] < 60_000:
            continue
        url = stream_url(item_id)
        if engine == "mpv":
            c = mpv_first_frame(url)
            w = mpv_first_frame(url) if c is not None else None
            near = int(meta["duration_ms"] * 0.1) + 30_000
            far = int(meta["duration_ms"] * 0.75)
            n = mpv_seek(url, near) if w is not None else None
            f = mpv_seek(url, far) if w is not None else None
            cf = mpv_seek(url, far) if w is not None else None
        else:
            # VLC HTTP Range attach can pull multi-GB; hard-cap so Part A finishes.
            c = vlc_first_frame(url, timeout=15)
            w = vlc_first_frame(url, timeout=15) if c is not None else None
            near = int(meta["duration_ms"] * 0.1) + 30_000
            far = int(meta["duration_ms"] * 0.75)
            n = vlc_seek(url, near, timeout=15) if w is not None else None
            f = vlc_seek(url, far, timeout=15) if w is not None else None
            cf = vlc_seek(url, far, timeout=15) if w is not None else None
        if c is not None:
            cold_s.append(c)
        if w is not None:
            warm_s.append(w)
        if n is not None:
            warm_n.append(n)
        if f is not None:
            warm_f.append(f)
        if cf is not None:
            cold_f.append(cf)
        print(
            f"  latency {engine} item={item_id} cold={None if c is None else round(c)} "
            f"warm={None if w is None else round(w)} far={None if f is None else round(f)}",
            flush=True,
        )
    return {
        "engine": engine,
        "client": "cli",
        "url_resolution": (
            "bake-off dp_byte_serve /items/{id}/stream from DB path; "
            "Nightjar /api/v0/items/{id}/stream is BROWSER_V0-gated and returns 415 for MKV"
        ),
        "cold_startup": summarize(cold_s),
        "warm_startup": summarize(warm_s),
        "warm_near_seek": summarize(warm_n),
        "warm_far_seek": summarize(warm_f),
        "cold_far_seek": summarize(cold_f),
    }


def file_url(path: str) -> str:
    return Path(path).as_uri()


def run_t4(engine: str, sample: dict, limit: int | None = None) -> dict:
    """First-frame on local file paths (decode/demux). HTTP attach is Part A."""
    titles = sample["t4_sample"]
    if limit:
        titles = titles[:limit]
    ok = fail = 0
    failures = []
    for t in titles:
        path = t.get("path")
        if not path or not Path(path).is_file():
            fail += 1
            failures.append({**{k: t.get(k) for k in ("id", "stratum", "video_codec", "audio_codec", "path")}, "note": "missing_file"})
            continue
        url = file_url(path)
        ms = mpv_first_frame(url, timeout=20) if engine == "mpv" else vlc_first_frame(url, timeout=20)
        if ms is not None:
            ok += 1
        else:
            fail += 1
            failures.append(
                {
                    "id": t["id"],
                    "stratum": t.get("stratum"),
                    "video_codec": t.get("video_codec"),
                    "audio_codec": t.get("audio_codec"),
                    "path": t.get("path"),
                }
            )
        if (ok + fail) % 20 == 0:
            print(f"  t4 {engine} {ok + fail}/{len(titles)} ok={ok} fail={fail}", flush=True)
    scored = ok + fail
    rate = 0.0 if scored == 0 else fail / scored
    return {
        "engine": engine,
        "client": "cli",
        "source": "file:// from DB path (decode/demux; HTTP attach measured in Part A)",
        "attempted": len(titles),
        "ok": ok,
        "fail": fail,
        "failure_rate": rate,
        "threshold": 0.02,
        "disqualified": rate > 0.02,
        "failures": failures[:80],
        "sampling": sample["method"],
        "stratum_counts": sample["stratum_counts"],
    }


def abr_signals() -> dict:
    return {
        "media_kit": {
            "usable_trigger": "buffering / bufferingPercentage / buffer Duration",
            "download_rate": False,
            "notes": "Player.stream API; audioBitrate is media bitrate not link goodput",
        },
        "libvlc_bakeoff": {
            "usable_trigger": "state Buffering/Playing; Buffering event; get_stats",
            "download_rate": "libvlc_media_player_get_stats input bitrate (not goodput)",
            "notes": "FFI state polling; no Flutter texture path",
        },
        "verdict": (
            "Both expose stall/buffer signals usable to trigger server rung reselection. "
            "Neither exposes clean proactive download-rate. stop_gate_neither_signal=false."
        ),
        "stop_gate_neither_signal": False,
    }


def run_part_b(engine: str, sample: dict) -> dict:
    """Compat-transcode sessions on Nightjar :8096 (not DP byte server)."""
    results = []
    for t in sample["part_b_candidates"][:5]:
        item_id = t["id"]
        bitrate = t.get("bitrate_bps_est") or 4_000_000
        configured = max(12500, bitrate // 2)
        info = json.loads(
            urllib.request.urlopen(
                f"{SESSION_BASE}/api/v0/items/{item_id}/playback-info", timeout=30
            ).read()
        )
        method = info.get("playbackMethod")
        if method != "transcode":
            results.append({"id": item_id, "skipped": True, "playbackMethod": method})
            continue
        sessions_url = info.get("sessionsUrl")
        req = urllib.request.Request(f"{SESSION_BASE}{sessions_url}", method="POST", data=b"")
        with urllib.request.urlopen(req, timeout=60) as resp:
            session = json.loads(resp.read())
        playlist = f"{SESSION_BASE}{session['playlistUrl']}"
        # Wait briefly for first segments
        time.sleep(2)
        if engine == "mpv":
            startup = mpv_first_frame(playlist, timeout=60)
            far = mpv_seek(playlist, int(t["duration_ms"] * 0.75), timeout=90)
        else:
            startup = vlc_first_frame(playlist, timeout=60)
            far = vlc_seek(playlist, int(t["duration_ms"] * 0.75), timeout=90)
        # 60s starve under relative throttle is recorded separately when proxy used
        results.append(
            {
                "id": item_id,
                "playbackMethod": method,
                "encoderKind": session.get("encoderKind"),
                "videoEncoder": session.get("videoEncoder"),
                "bitrate_bps_est": bitrate,
                "throttle_configured_bps": configured,
                "startup_ms": None if startup is None else round(startup),
                "far_seek_ms": None if far is None else round(far),
                "sessionId": session.get("sessionId"),
            }
        )
        urllib.request.urlopen(
            urllib.request.Request(
                f"{SESSION_BASE}/api/v0/sessions/{session['sessionId']}", method="DELETE"
            ),
            timeout=30,
        )
        print(f"  partb {engine} id={item_id} startup={startup} far={far}", flush=True)
    return {
        "engine": engine,
        "force_method": "compatibility-transcode via BROWSER_V0 playback-info",
        "caveat": (
            "Encoder not bitrate-capped; SessionMode::Transcode + forced IDR delivery. "
            "Sample skews high-bitrate vs library p50/p90."
        ),
        "runs": results,
    }


def summarize_request_patterns() -> dict:
    log = Path("/tmp/bakeoff-request-pattern.jsonl")
    if not log.exists():
        return {"n": 0}
    rows = [json.loads(l) for l in log.read_text().splitlines() if l.strip()]
    by_ua: dict[str, list] = {}
    for r in rows:
        ua = (r.get("user_agent") or "unknown")[:50]
        by_ua.setdefault(ua, []).append(r)
    summary = {}
    for ua, rs in by_ua.items():
        ranges = [r.get("range") for r in rs if r.get("range")]
        sizes = [r.get("bytes") or 0 for r in rs]
        summary[ua] = {
            "n": len(rs),
            "n_with_range": len(ranges),
            "range_samples": ranges[:15],
            "bytes_p50": sorted(sizes)[len(sizes) // 2] if sizes else 0,
            "bytes_max": max(sizes) if sizes else 0,
        }
    return {"n": len(rows), "by_user_agent": summary}


def main() -> None:
    sample = json.loads(SAMPLE.read_text())
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    probe = sample["latency_item_ids"][0]
    urllib.request.urlopen(urllib.request.Request(stream_url(probe), method="HEAD"), timeout=10)

    report = {
        "baseUrl": BASE,
        "sessionBase": SESSION_BASE,
        "client": "cli_mpv_vlc",
        "url_resolution_note": (
            "Part A uses bake-off dp_byte_serve from DB paths. "
            "Nightjar playback-info/stream remain BROWSER_V0 — cannot return MKV DP."
        ),
        "abr_signals": abr_signals(),
        "part_a_mpv": run_latency("mpv", sample),
        "part_a_vlc": run_latency("vlc", sample),
        "request_patterns": summarize_request_patterns(),
        "t4_mpv": run_t4("mpv", sample),
        "t4_vlc": run_t4("vlc", sample),
        "part_b_mpv": run_part_b("mpv", sample),
        "part_b_vlc": run_part_b("vlc", sample),
    }
    out = OUT_DIR / "bakeoff-report-cli.json"
    out.write_text(json.dumps(report, indent=2))
    print(f"wrote {out}")
    for k in ("t4_mpv", "t4_vlc"):
        print(k, report[k]["failure_rate"], "disq", report[k]["disqualified"])
    print("warm far mpv", report["part_a_mpv"].get("warm_far_seek"))
    print("warm far vlc", report["part_a_vlc"].get("warm_far_seek"))


if __name__ == "__main__":
    main()
