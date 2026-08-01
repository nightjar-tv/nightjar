#!/usr/bin/env python3
"""Gate 2 cold purge matrix after ADR-0023 handlers land on main.

Product-shaped I/O only:
  Matroska — HTTP Range header‖Cluster→EOF, no -ss (copy offset = Cluster PTS)
  MP4      — HTTP virtual faststart + -ss at map PTS

Forced `sudo purge` between every rep. n≥3, mid+far, both containers.
land_ms = wall to first non-empty HLS segment. Gate: land_ms < 3000.

Usage:
  PYTHONUNBUFFERED=1 python3 scripts/gate2_cold_purge_matrix.py
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from far_seek_cluster_spawn import (  # noqa: E402
    CASES,
    GATE_MS,
    UP,
    find_cluster_before,
    find_first_cluster,
    parse_sidx_ept,
    pick_encoder,
)
from far_seek_http_shim import spawn_hls_url, start_nas_shim  # noqa: E402
from mp4_virtual_faststart_spawn import (  # noqa: E402
    build_virtual_faststart,
    keyframe_near,
    start_server,
)

OUT_JSON = Path(
    os.environ.get(
        "OUT_JSON",
        "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/"
        "gate2-cold-purge-matrix-2026-08-01.json",
    )
)
OUT_MD = Path(
    os.environ.get(
        "OUT_MD",
        "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/"
        "gate2-cold-purge-matrix-2026-08-01.md",
    )
)
N = int(os.environ.get("N", "3"))
# End-moov WEBRip used by the handlers AAC gate; mid ~60s, far ~20m.
MP4 = Path(
    os.environ.get(
        "MP4",
        "/Volumes/media/TV Shows/Greys Anatomy/Season 6/"
        "Grey's Anatomy - 6x14 - Valentine's Day Massacre - WEBRip-1080p.mp4",
    )
)
MP4_CASES = [
    {"label": "mid", "ss_s": 60.0},
    {"label": "far", "ss_s": 1200.0},
]


def purge() -> dict:
    t0 = time.perf_counter()
    proc = subprocess.run(["sudo", "-n", "purge"], capture_output=True, text=True)
    if proc.returncode != 0:
        # Interactive sudo when passwordless is unavailable.
        proc = subprocess.run(["sudo", "purge"], capture_output=True, text=True)
    return {
        "ok": proc.returncode == 0,
        "ms": int((time.perf_counter() - t0) * 1000),
        "rc": proc.returncode,
        "err": (proc.stderr or "")[-200:],
    }


def spawn_mp4(url: str, mode: str, enc: str, ss_s: float) -> dict:
    out = Path(tempfile.mkdtemp(prefix="nj_g2_mp4_"))
    start_number = int(ss_s * 1000 // 2000)
    args = [
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-ss",
        f"{ss_s:.3f}",
        "-i",
        url,
        "-output_ts_offset",
        f"{ss_s:.3f}",
        "-map",
        "0:v:0",
        "-map",
        "0:a:0?",
    ]
    if mode == "copy":
        args += ["-c", "copy"]
    else:
        args += [
            "-c:v",
            enc,
            "-pix_fmt",
            "yuv420p",
            "-force_key_frames",
            "expr:gte(t,n_forced*2.0)",
            "-g",
            "600",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-b:a",
            "128k",
        ]
    args += [
        "-f",
        "hls",
        "-hls_time",
        "2",
        "-hls_segment_type",
        "fmp4",
        "-hls_flags",
        "independent_segments",
        "-start_number",
        str(start_number),
        "-hls_segment_filename",
        str(out / "seg_%05d.m4s"),
        "-t",
        "16",
        str(out / "index.m3u8"),
    ]
    t0 = time.perf_counter()
    proc = subprocess.Popen(args, stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
    first_seg = None
    first_ms = None
    deadline = t0 + 120
    while time.perf_counter() < deadline:
        segs = sorted(out.glob("seg_*.m4s"))
        if segs and segs[0].stat().st_size > 0:
            first_seg = segs[0]
            first_ms = int((time.perf_counter() - t0) * 1000)
            break
        if proc.poll() is not None:
            break
        time.sleep(0.05)
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    err = ""
    if proc.stderr:
        err = proc.stderr.read().decode("utf-8", "replace")[-800:]
    sidx = parse_sidx_ept(first_seg) if first_seg else None
    return {
        "land_ms": first_ms,
        "sidx_ept_s": sidx,
        "first_seg": first_seg.name if first_seg else None,
        "out_dir": str(out),
        "err_tail": err,
        "rc": proc.returncode,
    }


def run_matroska(enc: str) -> list[dict]:
    rows = []
    header_end = find_first_cluster(UP)
    for case in CASES:
        cluster = find_cluster_before(UP, case["pkt_pos"])
        with open(UP, "rb") as f:
            header = f.read(header_end)
        # Cluster PTS for copy offset (ADR-0023 / http-shim honesty).
        synth_tmp = Path(tempfile.mkstemp(prefix="nj_pts_", suffix=".mkv")[1])
        with open(UP, "rb") as f:
            f.seek(cluster)
            body = f.read(4 * 1024 * 1024)
        synth_tmp.write_bytes(header + body)
        pr = subprocess.run(
            [
                "ffprobe",
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time",
                "-of",
                "csv=p=0",
                "-read_intervals",
                "%+#1",
                str(synth_tmp),
            ],
            capture_output=True,
            text=True,
        )
        synth_tmp.unlink(missing_ok=True)
        cluster_pts = float(pr.stdout.strip().splitlines()[0].split(",")[0])
        for mode in ("transcode", "copy"):
            offset = case["ss_s"] if mode == "transcode" else cluster_pts
            for rep in range(N):
                print(f"mkv {case['label']} {mode} rep{rep} purge…", flush=True)
                p = purge()
                if not p["ok"]:
                    raise SystemExit(f"sudo purge failed: {p}")
                httpd, url, H = start_nas_shim(header, UP, cluster)
                try:
                    hls = spawn_hls_url(url, mode, enc, offset)
                finally:
                    httpd.shutdown()
                land = hls.get("land_ms")
                sidx = hls.get("sidx_ept_s")
                row = {
                    "container": "matroska",
                    "shape": "http_nas_remainder",
                    "src": str(UP),
                    "label": case["label"],
                    "mode": mode,
                    "rep": rep,
                    "requested_s": case["ss_s"],
                    "offset_s": offset,
                    "cluster_pts_s": cluster_pts,
                    "land_ms": land,
                    "under_3s": bool(land is not None and land < GATE_MS),
                    "sidx_ept_s": sidx,
                    "delta_vs_request": None if sidx is None else sidx - case["ss_s"],
                    "purge": p,
                    "http_range_hits": H.range_hits,
                    "err_tail": (hls.get("err_tail") or "")[:400],
                }
                rows.append(row)
                print(json.dumps({k: row[k] for k in row if k != "err_tail"}), flush=True)
                shutil.rmtree(hls["out_dir"], ignore_errors=True)
    return rows


def run_mp4(enc: str) -> list[dict]:
    rows = []
    if not MP4.is_file():
        raise SystemExit(f"missing MP4 {MP4}")
    v = build_virtual_faststart(MP4)
    if v["kind"] != "end_moov_virtual_faststart":
        raise SystemExit(f"expected end-moov, got {v['kind']}")
    httpd, url, H = start_server(v, MP4)
    try:
        for case in MP4_CASES:
            kf = keyframe_near(MP4, case["ss_s"])
            for mode in ("transcode", "copy"):
                for rep in range(N):
                    print(f"mp4 {case['label']} {mode} rep{rep} purge…", flush=True)
                    p = purge()
                    if not p["ok"]:
                        raise SystemExit(f"sudo purge failed: {p}")
                    H.bytes_served = 0
                    H.range_hits = 0
                    hls = spawn_mp4(url, mode, enc, case["ss_s"])
                    land = hls.get("land_ms")
                    sidx = hls.get("sidx_ept_s")
                    advertise = case["ss_s"] if mode == "transcode" else kf.get("pts_s")
                    row = {
                        "container": "mp4",
                        "shape": "virtual_faststart_http_ss",
                        "src": str(MP4),
                        "label": case["label"],
                        "mode": mode,
                        "rep": rep,
                        "requested_s": case["ss_s"],
                        "map_pts_s": kf.get("pts_s"),
                        "land_ms": land,
                        "under_3s": bool(land is not None and land < GATE_MS),
                        "sidx_ept_s": sidx,
                        "delta_vs_advertise": None
                        if sidx is None or advertise is None
                        else sidx - advertise,
                        "purge": p,
                        "http_range_hits": H.range_hits,
                        "err_tail": (hls.get("err_tail") or "")[:400],
                    }
                    rows.append(row)
                    print(
                        json.dumps({k: row[k] for k in row if k != "err_tail"}),
                        flush=True,
                    )
                    shutil.rmtree(hls["out_dir"], ignore_errors=True)
    finally:
        httpd.shutdown()
    return rows


def write_md(rows: list[dict], enc: str, main_sha: str) -> None:
    lines = [
        "# Gate 2 cold purge matrix (2026-08-01)",
        "",
        "Post-merge ADR-0023 handlers. Product-shaped HTTP virtual inputs,",
        f"`sudo purge` before every rep, n={N}, gate land_ms < {GATE_MS}.",
        "",
        f"- main: `{main_sha}`",
        f"- encoder: `{enc}`",
        f"- raw: `{OUT_JSON.name}`",
        f"- harness: `scripts/gate2_cold_purge_matrix.py`",
        "",
        "## Results",
        "",
        "| container | land | mode | land_ms (n reps) | max | under 3s |",
        "|---|---|---|---|---:|:---:|",
    ]
    from collections import defaultdict

    groups: dict[tuple, list] = defaultdict(list)
    for r in rows:
        groups[(r["container"], r["label"], r["mode"])].append(r)
    all_ok = True
    for key in sorted(groups):
        rs = groups[key]
        lands = [r["land_ms"] for r in rs if r["land_ms"] is not None]
        ok = all(r["under_3s"] for r in rs) and len(lands) == N
        all_ok = all_ok and ok
        lands_s = "/".join(str(x) for x in lands) if lands else "—"
        mx = max(lands) if lands else None
        lines.append(
            f"| {key[0]} | {key[1]} | {key[2]} | {lands_s} | {mx} | {'yes' if ok else 'NO'} |"
        )
    lines += [
        "",
        f"**Verdict:** {'PASS — every cold land under 3 s' if all_ok else 'FAIL — see raw JSON'}.",
        "",
        "Matroska shape matches production Cluster splice (no `-ss`).",
        "MP4 shape matches production virtual faststart (keeps `-ss` at map PTS).",
        "Live replace under an open session remains a manual Unraid dogfood step",
        "(ADR-0023 §8); not covered by this matrix.",
        "",
    ]
    OUT_MD.write_text("\n".join(lines))
    print("wrote", OUT_MD, flush=True)


def main() -> None:
    enc = pick_encoder()
    main_sha = (
        subprocess.check_output(["git", "rev-parse", "--short", "HEAD"], text=True).strip()
    )
    print(f"encoder={enc} HEAD={main_sha} N={N}", flush=True)
    if not UP.is_file():
        raise SystemExit(f"missing {UP}")
    rows = []
    rows.extend(run_matroska(enc))
    rows.extend(run_mp4(enc))
    OUT_JSON.parent.mkdir(parents=True, exist_ok=True)
    doc = {
        "main_sha": main_sha,
        "encoder": enc,
        "n": N,
        "gate_ms": GATE_MS,
        "note": (
            "Forced sudo purge before every rep. Product-shaped HTTP virtual "
            "inputs only (Matroska remainder + MP4 virtual faststart)."
        ),
        "rows": rows,
    }
    OUT_JSON.write_text(json.dumps(doc, indent=2))
    print("wrote", OUT_JSON, flush=True)
    write_md(rows, enc, main_sha)
    fails = [r for r in rows if not r["under_3s"]]
    if fails:
        raise SystemExit(f"{len(fails)} cold lands failed the 3s gate")
    print("ALL UNDER 3s", flush=True)


if __name__ == "__main__":
    main()
