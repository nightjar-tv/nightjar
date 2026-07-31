#!/usr/bin/env python3
"""Step 1c: does forced-IDR transcode land on the N × hls_time grid?

Compares produced fMP4 sidx earliest_presentation_time against the
title-absolute grid implied by ADR-0008 §3 (-force_key_frames + scenecut off),
not against source-keyframe prediction (that is the copy-mode instrument).

Usage:
  python3 scripts/t1c_transcode_cut_grid.py
"""

from __future__ import annotations

import json
import re
import struct
import subprocess
import tempfile
from pathlib import Path

OUT_JSON = Path("notes/client-arch/transcode-cut-grid-2026-07-31.json")
OUT_MD = Path("notes/client-arch/transcode-cut-grid-2026-07-31.md")
TOL_S = 0.050
ENCODE_S = 40.0

CASES = [
    {
        "label": "Elementary_3x05_longGOP",
        "path": "/Volumes/media/TV Shows/Elementary/Season 3/Elementary - 3x05 - Rip Off - WEBDL-1080p.mkv",
        "shape": "long-GOP",
        "starts_s": [0.0, 300.0, 600.0, 1200.0],
    },
    {
        "label": "RickMorty_9x04_shortGOP",
        "path": "/Volumes/media/TV Shows/Rick and Morty/Season 9/Rick and Morty - 9x04 - A Ricker Runs Through It - WEBDL-1080p.mkv",
        "shape": "short-GOP",
        "starts_s": [0.0, 120.0, 300.0],
    },
    {
        "label": "AngryMen_feature",
        "path": "/Volumes/media/Movies/12 Angry Men (1957)/12 Angry Men (1957) Bluray-1080p.mkv",
        "shape": "feature",
        "starts_s": [0.0, 900.0, 1800.0],
    },
    {
        "label": "Futurama_4x06_8512",
        "path": "/Volumes/media/TV Shows/Futurama/Season 4/Futurama - 4x06 - Where the Buggalo Roam - DVD.mkv",
        "shape": "damaged-DVD",
        "starts_s": [0.0, 300.0],
    },
    {
        "label": "corpus_vfr_mp4",
        "path": "testdata/files/h264_aac_vfr_mp4.mp4",
        "shape": "VFR",
        "starts_s": [0.0],
    },
]

HLS_TIMES = (2.0, 10.0)


def run(cmd: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True)


def sidx_ms(data: bytes) -> int | None:
    off = 0
    while off + 8 <= len(data):
        size = struct.unpack(">I", data[off : off + 4])[0]
        typ = data[off + 4 : off + 8]
        if size == 1:
            size = struct.unpack(">Q", data[off + 8 : off + 16])[0]
        if size < 8 or off + size > len(data):
            break
        if typ == b"sidx":
            ver = data[off + 8]
            timescale = struct.unpack(">I", data[off + 16 : off + 20])[0]
            if ver == 0:
                earliest = struct.unpack(">I", data[off + 20 : off + 24])[0]
            else:
                earliest = struct.unpack(">Q", data[off + 20 : off + 28])[0]
            if timescale:
                return int(round(earliest * 1000 / timescale))
            return None
        off += size
    return None


def expected_grid(start_s: float, hls_time: float, n: int) -> list[float]:
    # Title-absolute: land at start_s (encode -ss + -output_ts_offset), then
    # forced IDRs every hls_time of encode timeline → start_s + i * hls_time.
    return [start_s + i * hls_time for i in range(n)]


def produce(path: str, start_s: float, hls_time: float, work: Path) -> list[float]:
    work.mkdir(parents=True, exist_ok=True)
    for f in work.glob("*"):
        if f.is_file():
            f.unlink()
    force = f"expr:gte(t,n_forced*{hls_time:g})"
    cmd = [
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-ss",
        f"{start_s:.3f}",
        "-i",
        path,
        "-output_ts_offset",
        f"{start_s:.3f}",
        "-map",
        "0:v:0",
        "-map",
        "0:a:0?",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-force_key_frames",
        force,
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
        "-t",
        f"{ENCODE_S:g}",
        "-f",
        "hls",
        "-hls_time",
        f"{hls_time:g}",
        "-hls_list_size",
        "0",
        "-hls_flags",
        "independent_segments+temp_file",
        "-hls_segment_type",
        "fmp4",
        "-hls_fmp4_init_filename",
        "init.mp4",
        "-hls_segment_filename",
        str(work / "seg%d.m4s"),
        str(work / "index.m3u8"),
    ]
    proc = run(cmd)
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr[:500] or proc.stdout[:500] or "ffmpeg failed")
    init = (work / "init.mp4").read_bytes()
    times: list[float] = []
    for seg in sorted(work.glob("seg*.m4s"), key=lambda p: int(re.findall(r"\d+", p.stem)[0])):
        ms = sidx_ms(init + seg.read_bytes())
        if ms is not None:
            times.append(ms / 1000.0)
    return times


def main() -> int:
    root = Path(tempfile.mkdtemp(prefix="nj-t1c-tcgrid-"))
    results = []
    for case in CASES:
        path = case["path"]
        if not Path(path).is_file():
            results.append({**case, "error": "missing"})
            print(f"MISSING {case['label']}", flush=True)
            continue
        for hls_time in HLS_TIMES:
            for start_s in case["starts_s"]:
                label = f"{case['label']}_h{hls_time:g}_s{start_s:g}"
                print(f"run {label}", flush=True)
                try:
                    actual = produce(
                        path, start_s, hls_time, root / label
                    )
                except Exception as e:  # noqa: BLE001 — instrument
                    results.append(
                        {
                            "label": case["label"],
                            "shape": case["shape"],
                            "hls_time": hls_time,
                            "start_s": start_s,
                            "error": str(e),
                        }
                    )
                    print(f"  ERROR {e}", flush=True)
                    continue
                expect = expected_grid(start_s, hls_time, len(actual))
                pairs = []
                mism = 0
                for i, (want, got) in enumerate(zip(expect, actual)):
                    d = abs(want - got)
                    ok = d <= TOL_S
                    if not ok:
                        mism += 1
                    pairs.append(
                        {
                            "i": i,
                            "want": want,
                            "got": got,
                            "delta_s": d,
                            "ok": ok,
                        }
                    )
                row = {
                    "label": case["label"],
                    "shape": case["shape"],
                    "hls_time": hls_time,
                    "start_s": start_s,
                    "actual_n": len(actual),
                    "compared": len(pairs),
                    "mismatches": mism,
                    "mismatch_rate": (mism / len(pairs) if pairs else None),
                    "max_abs_delta_s": max((p["delta_s"] for p in pairs), default=None),
                    "first_fail": next((p for p in pairs if not p["ok"]), None),
                    "pairs_head": pairs[:8],
                }
                results.append(row)
                print(
                    f"  {mism}/{len(pairs)} maxΔ={row['max_abs_delta_s']}",
                    flush=True,
                )

    ok_rows = [r for r in results if "error" not in r]
    tot_c = sum(r["compared"] for r in ok_rows)
    tot_m = sum(r["mismatches"] for r in ok_rows)
    by_hls: dict = {}
    by_shape: dict = {}
    for r in ok_rows:
        for key, bucket in (
            (r["hls_time"], by_hls),
            (r["shape"], by_shape),
        ):
            bucket.setdefault(key, {"compared": 0, "mismatches": 0})
            bucket[key]["compared"] += r["compared"]
            bucket[key]["mismatches"] += r["mismatches"]
    for bucket in (by_hls, by_shape):
        for v in bucket.values():
            v["rate"] = v["mismatches"] / v["compared"] if v["compared"] else None

    summary = {
        "question": "Forced-IDR transcode segment starts match N×hls_time grid?",
        "tol_s": TOL_S,
        "encode_window_s": ENCODE_S,
        "model": "want[i] = start_s + i*hls_time; produce with -force_key_frames expr + -sc_threshold 0",
        "total_compared": tot_c,
        "total_mismatches": tot_m,
        "overall_mismatch_rate": (tot_m / tot_c if tot_c else None),
        "by_hls_time": {str(k): v for k, v in by_hls.items()},
        "by_shape": by_shape,
        "results": results,
    }
    OUT_JSON.parent.mkdir(parents=True, exist_ok=True)
    OUT_JSON.write_text(json.dumps(summary, indent=2))

    lines = [
        "# Transcode cut grid (Step 1c)",
        "",
        "- Date: 2026-07-31",
        f"- Tolerance: **{TOL_S*1000:.0f} ms** on sidx earliest_presentation_time",
        f"- Encode window: {ENCODE_S:g}s; hls_time ∈ {list(HLS_TIMES)}",
        "- Args: libx264 ultrafast, `-force_key_frames expr:gte(t,n_forced*H)`,",
        "  `-g 600 -keyint_min 48 -sc_threshold 0` (ADR-0008 §3 floor)",
        "",
        "## Headline",
        "",
        f"| compared | mismatches | rate |",
        f"|---:|---:|---:|",
        f"| {tot_c} | {tot_m} | {summary['overall_mismatch_rate']} |",
        "",
        "### By hls_time",
        "",
        "```json",
        json.dumps(summary["by_hls_time"], indent=2),
        "```",
        "",
        "### By shape",
        "",
        "```json",
        json.dumps(by_shape, indent=2),
        "```",
        "",
        "## Verdict",
        "",
    ]
    if tot_c and tot_m == 0:
        lines += [
            "**Zero mismatches.** Forced-IDR transcode boundaries match the",
            "uniform grid. An honest full-title playlist is publishable for",
            "transcode sessions; the scrubbing ceiling lifts on the bandwidth",
            "session path (remote), where it still matters.",
            "",
        ]
    else:
        lines += [
            f"**Non-zero residual ({tot_m}/{tot_c}).** Do not treat full-title",
            "transcode playlists as load-bearing until the residual is explained.",
            "",
        ]
    lines.append(f"Raw: `{OUT_JSON}`.")
    OUT_MD.write_text("\n".join(lines) + "\n")
    print(json.dumps({k: summary[k] for k in summary if k != "results"}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
