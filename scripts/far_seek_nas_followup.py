#!/usr/bin/env python3
"""Far-seek follow-ups (2026-08-01): cold mid/far asymmetry, minimal probe, SMB read.

Same title (Up Bluray NAS), same offsets as latency_exp2_exp4. No Nightjar.
"""
from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

UP = Path("/Volumes/media/Movies/Up (2009)/Up (2009) Bluray-1080p.mkv")
SEG_MS = 2000
LEAD = 8
OUT = Path(
    os.environ.get(
        "OUT",
        "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/far-seek-nas-followup-2026-08-01.json",
    )
)
N = int(os.environ.get("N", "3"))
MID_MS = 60_000
FAR_MS = 2_196_000


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


def seek_only(src: Path, start_ms: int, extra_in: list[str] | None = None) -> int:
    args = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y"]
    if extra_in:
        args += extra_in
    args += [
        "-ss",
        f"{start_ms / 1000:.3f}",
        "-i",
        str(src),
        "-frames:v",
        "1",
        "-f",
        "null",
        "-",
    ]
    t0 = time.perf_counter()
    subprocess.run(args, check=False, capture_output=True)
    return int((time.perf_counter() - t0) * 1000)


def hls_milestones(
    src: Path,
    play_ms: int,
    encoder: str,
    *,
    label: str,
    extra_in: list[str] | None = None,
) -> dict:
    play_start = (play_ms // SEG_MS) * SEG_MS
    start_ms = max(0, play_start - LEAD * SEG_MS)
    window = start_ms // SEG_MS
    land = play_start // SEG_MS
    out = Path(tempfile.mkdtemp(prefix="nj_lat_"))
    force_kf = f"expr:gte(t,n_forced*{SEG_MS / 1000})"
    args = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y"]
    if extra_in:
        args += extra_in
    if start_ms:
        args += ["-ss", f"{start_ms / 1000:.3f}"]
    args += ["-i", str(src)]
    if start_ms:
        args += ["-output_ts_offset", f"{start_ms / 1000:.3f}"]
    progress = out / "progress.txt"
    args += [
        "-progress",
        str(progress),
        "-map",
        "0:v:0",
        "-map",
        "0:a:0",
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
    marks: dict = {
        "spawn_return_ms": int((time.perf_counter() - t0) * 1000),
        "first_progress_ms": None,
        "init_ms": None,
        "seg_times_ms": {},
    }
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
    import shutil

    shutil.rmtree(out, ignore_errors=True)
    return {
        "label": label,
        "play_ms": play_ms,
        "encode_start_ms": start_ms,
        "seek_only_ms": seek_only(src, start_ms, extra_in),
        "spawn_return_ms": marks["spawn_return_ms"],
        "first_progress_ms": marks["first_progress_ms"],
        "init_ms": marks["init_ms"],
        "first_window_seg_ms": segs.get(window),
        "land_seg_ms": segs.get(land),
        "lead_encode_ms": (
            (segs[land] - segs[window]) if window in segs and land in segs else None
        ),
    }


def stats(rows: list[dict], key: str) -> dict:
    vals = sorted(r[key] for r in rows if r.get(key) is not None)
    if not vals:
        return {"n": 0}
    return {
        "n": len(vals),
        "min": vals[0],
        "p50": vals[(len(vals) - 1) // 2],
        "max": vals[-1],
    }


def smb_mid_read(src: Path, nbytes: int = 5 * 1024 * 1024) -> dict:
    size = src.stat().st_size
    offset = size // 2
    # Three cold-ish reads: reopen each time. OS cache may still warm.
    samples = []
    for i in range(N):
        t0 = time.perf_counter()
        with open(src, "rb") as f:
            f.seek(offset)
            got = f.read(nbytes)
        ms = int((time.perf_counter() - t0) * 1000)
        samples.append({"i": i, "ms": ms, "bytes": len(got), "offset": offset})
        print(json.dumps({"phase": "smb_read", **samples[-1]}))
        time.sleep(0.5)
    return {
        "path": str(src),
        "nbytes": nbytes,
        "offset": offset,
        "size": size,
        "samples": samples,
        "stats_ms": stats([{"ms": s["ms"]} for s in samples], "ms"),
    }


def main():
    if not UP.exists():
        raise SystemExit(f"missing {UP}")
    enc = pick_encoder()
    print(json.dumps({"encoder": enc, "src": str(UP), "N": N}))

    # --- 1. Cold mid vs far, n>=3, interleaved to surface cache effects ---
    cold = []
    order = []
    for i in range(N):
        order.append(("mid", MID_MS, i))
        order.append(("far", FAR_MS, i))
    # Also a block of mid-only then far-only at the end for order contrast
    for i in range(N):
        order.append(("mid_block", MID_MS, i))
    for i in range(N):
        order.append(("far_block", FAR_MS, i))

    for kind, play, i in order:
        label = f"default_{kind}_{i}"
        r = hls_milestones(UP, play, enc, label=label)
        r["kind"] = kind
        r["rep"] = i
        r["extra"] = "default"
        cold.append(r)
        print(json.dumps(r))
        time.sleep(1)

    # --- 2. Minimal probe + known container/maps (DB: matroska, 0:v:0, 0:a:0) ---
    # probesize/analyzeduration at practical floor; -f matroska so demuxer is known.
    min_probe = [
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-f",
        "matroska",
    ]
    probed = []
    for kind, play in (("mid", MID_MS), ("far", FAR_MS)):
        for i in range(N):
            label = f"minprobe_{kind}_{i}"
            r = hls_milestones(UP, play, enc, label=label, extra_in=min_probe)
            r["kind"] = kind
            r["rep"] = i
            r["extra"] = "min_probe_matroska"
            probed.append(r)
            print(json.dumps(r))
            time.sleep(1)

    # --- 3. Plain 5 MiB mid-file SMB read ---
    smb = smb_mid_read(UP)

    def summarize(rows, kinds):
        out = {}
        for k in kinds:
            subset = [r for r in rows if r["kind"] == k or r["kind"].startswith(k)]
            # For interleaved, kind is exactly mid/far; for blocks mid_block/far_block
            exact = [r for r in rows if r["kind"] == k]
            use = exact if exact else subset
            out[k] = {
                "seek_only": stats(use, "seek_only_ms"),
                "first_progress": stats(use, "first_progress_ms"),
                "land": stats(use, "land_seg_ms"),
            }
        return out

    report = {
        "stamp": "2026-08-01",
        "src": str(UP),
        "encoder": enc,
        "held": {
            "gate_3s": "hold — local clears; NAS is the product case",
            "source_swap": "killed — ~60ms POST + ~15ms GET when mapped; full-title+cook-on-miss is not the 7s fix",
            "seek_only_nas": "1.7–3.1s before any output — kill/respawn pays network too",
        },
        "cold_default": {
            "rows": cold,
            "by_kind": summarize(
                cold, ["mid", "far", "mid_block", "far_block"]
            ),
        },
        "min_probe": {
            "flags": min_probe,
            "rows": probed,
            "by_kind": summarize(probed, ["mid", "far"]),
        },
        "smb_5mb_mid": smb,
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(report, indent=2))
    print(json.dumps({"wrote": str(OUT)}, indent=2))

    print("\n=== cold default (interleaved mid/far) ===")
    for k in ("mid", "far", "mid_block", "far_block"):
        s = report["cold_default"]["by_kind"][k]
        print(
            f"{k:10} seek_only p50={s['seek_only'].get('p50')} "
            f"1stProg p50={s['first_progress'].get('p50')} "
            f"land p50={s['land'].get('p50')}"
        )
    print("\n=== min probe + -f matroska ===")
    for k in ("mid", "far"):
        s = report["min_probe"]["by_kind"][k]
        print(
            f"{k:10} seek_only p50={s['seek_only'].get('p50')} "
            f"1stProg p50={s['first_progress'].get('p50')} "
            f"land p50={s['land'].get('p50')}"
        )
    print("\n=== smb 5MiB mid-file ===")
    print(json.dumps(smb["stats_ms"]))


if __name__ == "__main__":
    main()
