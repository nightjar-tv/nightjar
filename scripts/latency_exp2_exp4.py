#!/usr/bin/env python3
"""Latency investigation: Experiments 2 and 4 (measurement only).

Breaks mid-title session startup into seek / init / sequential segments /
land, on local SSD vs NAS, mirroring nightjar's ffmpeg flags.

Also answers Exp 2: whether land is blocked by serial encode order.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

UP = Path("/Volumes/media/Movies/Up (2009)/Up (2009) Bluray-1080p.mkv")
WHIP = Path("/Volumes/media/Movies/Whiplash (2014)/Whiplash (2014) Bluray-1080p.mkv")
LOCAL = Path("/tmp/nj_local_long.mkv")
SEG_MS = 2000
LEAD = 8


def ensure_local():
    if LOCAL.exists():
        return
    subprocess.check_call(
        [
            "ffmpeg",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=1280x720:rate=24:duration=120",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=120",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-ac",
            "2",
            str(LOCAL),
        ]
    )


def pick_encoder() -> str:
    for enc in ("h264_videotoolbox", "libx264"):
        p = subprocess.run(
            [
                "ffmpeg",
                "-hide_banner",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=0.1",
                "-c:v",
                enc,
                "-f",
                "null",
                "-",
            ],
            capture_output=True,
        )
        if p.returncode == 0:
            return enc
    raise SystemExit("no encoder")


def seek_only(src: Path, start_ms: int) -> int:
    t0 = time.perf_counter()
    subprocess.run(
        [
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            f"{start_ms / 1000:.3f}",
            "-i",
            str(src),
            "-frames:v",
            "1",
            "-f",
            "null",
            "-",
        ],
        check=False,
        capture_output=True,
    )
    return int((time.perf_counter() - t0) * 1000)


def hls_milestones(src: Path, play_ms: int, encoder: str, audio_map: str = "0:a:0") -> dict:
    play_start = (play_ms // SEG_MS) * SEG_MS
    start_ms = max(0, play_start - LEAD * SEG_MS)
    window = start_ms // SEG_MS
    land = play_start // SEG_MS
    out = Path(tempfile.mkdtemp(prefix="nj_lat_"))
    force_kf = f"expr:gte(t,n_forced*{SEG_MS/1000})"
    args = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y"]
    if start_ms:
        args += ["-ss", f"{start_ms/1000:.3f}"]
    args += ["-i", str(src)]
    if start_ms:
        args += ["-output_ts_offset", f"{start_ms/1000:.3f}"]
    # Progress file for "first output packet" proxy (out_time_ms updates).
    progress = out / "progress.txt"
    args += [
        "-progress",
        str(progress),
        "-map",
        "0:v:0",
        "-map",
        audio_map,
        "-c:v",
        encoder,
        "-pix_fmt",
        "yuv420p",
        "-map_metadata",
        "-1",
        "-vf",
        "sidedata=delete,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-force_key_frames",
        force_kf,
        "-g",
        "600",
        "-keyint_min",
        "48",
        "-sc_threshold",
        "0",
        "-c:a",
        "aac",
        "-ac",
        "2",
        "-b:a",
        "128k",
        "-f",
        "hls",
        "-hls_time",
        str(SEG_MS / 1000),
        "-hls_segment_type",
        "fmp4",
        "-hls_flags",
        "independent_segments",
        "-start_number",
        str(window),
        "-hls_segment_filename",
        str(out / "seg%03d.m4s"),
        "-t",
        "24",
        str(out / "index.m3u8"),
    ]
    t0 = time.perf_counter()
    proc = subprocess.Popen(args)
    marks: dict[str, int | None] = {
        "spawn_return_ms": None,  # process created
        "first_progress_ms": None,
        "init_ms": None,
        "seg_times_ms": {},  # idx -> ms
    }
    marks["spawn_return_ms"] = int((time.perf_counter() - t0) * 1000)
    deadline = t0 + 90
    while time.perf_counter() < deadline:
        now_ms = int((time.perf_counter() - t0) * 1000)
        if marks["first_progress_ms"] is None and progress.exists():
            try:
                txt = progress.read_text()
                if "out_time_ms=" in txt or "progress=" in txt:
                    marks["first_progress_ms"] = now_ms
            except OSError:
                pass
        if marks["init_ms"] is None and (out / "init.mp4").exists():
            if (out / "init.mp4").stat().st_size > 0:
                marks["init_ms"] = now_ms
        for idx in range(window, land + 1):
            key = str(idx)
            if key in marks["seg_times_ms"]:
                continue
            p = out / f"seg{idx:03d}.m4s"
            if p.exists() and p.stat().st_size > 0:
                marks["seg_times_ms"][key] = now_ms
        if str(land) in marks["seg_times_ms"] and marks["init_ms"] is not None:
            break
        if proc.poll() is not None and str(window) not in marks["seg_times_ms"]:
            break
        time.sleep(0.05)
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    segs = {int(k): v for k, v in marks["seg_times_ms"].items()}
    # Serial encode proof: each idx appears only after idx-1.
    order = [segs[i] for i in range(window, land + 1) if i in segs]
    strictly_increasing = all(a < b for a, b in zip(order, order[1:]))
    gaps = [b - a for a, b in zip(order, order[1:])]
    result = {
        "src": src.name,
        "play_ms": play_ms,
        "encode_start_ms": start_ms,
        "play_start_ms": play_start,
        "window_idx": window,
        "land_idx": land,
        "lead_segments": land - window,
        "seek_only_ms": seek_only(src, start_ms) if start_ms else seek_only(src, 0),
        "spawn_return_ms": marks["spawn_return_ms"],
        "first_progress_ms": marks["first_progress_ms"],
        "init_ms": marks["init_ms"],
        "first_window_seg_ms": segs.get(window),
        "second_seg_ms": segs.get(window + 1),
        "land_seg_ms": segs.get(land),
        "seg_times_ms": segs,
        "serial_encode": strictly_increasing,
        "inter_seg_gaps_ms": gaps,
        "lead_encode_ms": (
            (segs[land] - segs[window]) if window in segs and land in segs else None
        ),
    }
    shutil.rmtree(out, ignore_errors=True)
    return result


def main():
    ensure_local()
    enc = pick_encoder()
    print(json.dumps({"encoder": enc}))
    cases = [
        ("local_mid", LOCAL, 60_000, "0:a:0"),
        ("nas_up_mid_60s", UP, 60_000, "0:a:0"),
        ("nas_up_chrome_seek", UP, 2_196_000, "0:a:0"),
        ("nas_whip_switch", WHIP, 2_516_000, "0:a:1"),
        ("nas_whip_cold", WHIP, 0, "0:a:0"),
        ("nas_up_cold", UP, 0, "0:a:0"),
    ]
    rows = []
    for label, src, play, amap in cases:
        if not src.exists():
            print(json.dumps({"label": label, "error": "missing"}))
            continue
        r = hls_milestones(src, play, enc, amap)
        r["label"] = label
        rows.append(r)
        print(json.dumps(r))
    out = Path("/tmp/nj-latency-exp2-exp4.json")
    out.write_text(json.dumps({"encoder": enc, "rows": rows}, indent=2))
    print(f"wrote {out}")
    # Summary table
    print("\nPhase breakdown (ms):")
    hdr = f"{'label':22} {'seek':>6} {'1stProg':>7} {'init':>6} {'first':>6} {'2nd':>6} {'land':>6} {'leadEnc':>7} {'serial':>6}"
    print(hdr)
    for r in rows:
        print(
            f"{r['label']:22} {r.get('seek_only_ms') or '-':>6} "
            f"{r.get('first_progress_ms') or '-':>7} {r.get('init_ms') or '-':>6} "
            f"{r.get('first_window_seg_ms') or '-':>6} {r.get('second_seg_ms') or '-':>6} "
            f"{r.get('land_seg_ms') or '-':>6} {r.get('lead_encode_ms') or '-':>7} "
            f"{str(r.get('serial_encode')):>6}"
        )


if __name__ == "__main__":
    main()
