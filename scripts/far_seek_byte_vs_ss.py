#!/usr/bin/env python3
"""Separate cold-open mechanisms: byte-range pattern vs -ss vs byte-offset jump.

1. ffmpeg -loglevel debug on cold -ss → ordered seek/read positions from log
2. A/B: -ss TIME vs -skip_initial_bytes POS -f matroska (known KF from Cues/packets)
3. Optional: plain pread scatter if we parse debug ranges

Same file as prior NAS follow-up (Up Bluray).
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import time
from pathlib import Path

UP = Path("/Volumes/media/Movies/Up (2009)/Up (2009) Bluray-1080p.mkv")
OUT = Path(
    os.environ.get(
        "OUT",
        "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/far-seek-byte-vs-ss-2026-08-01.json",
    )
)
N = int(os.environ.get("N", "3"))

# From ffprobe packet dump (key frames):
# mid ~44.461s @ byte 66136568; far ~2177.090s @ byte 2920900180
CASES = [
    {
        "label": "mid",
        "ss_s": 44.461,
        "byte_pos": 66_136_568,
    },
    {
        "label": "far",
        "ss_s": 2177.090,
        "byte_pos": 2_920_900_180,
    },
]

# seek/read lines from lavf / aviobuf debug
SEEK_RE = re.compile(
    r"(?:Seek(?:ing)? (?:to|from)|seek to|avio_seek|pos[= ]|skip_initial_bytes)",
    re.I,
)
POS_RE = re.compile(
    r"(?:pos[=:]?\s*|seek(?:ing)? (?:to|from)\s*|bytes?\s+)(\d{4,})",
    re.I,
)


def run_ffmpeg(args: list[str], timeout: int = 120) -> tuple[int, str, int]:
    t0 = time.perf_counter()
    p = subprocess.run(
        args,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    ms = int((time.perf_counter() - t0) * 1000)
    err = p.stderr or ""
    return p.returncode, err, ms


def parse_debug_io(stderr: str) -> dict:
    """Extract ordered numeric positions mentioned on seek/read-ish lines."""
    events = []
    for line in stderr.splitlines():
        if not SEEK_RE.search(line) and "avio" not in line.lower():
            # still catch matroska cue seeks
            if "cues" not in line.lower() and "cluster" not in line.lower():
                continue
        positions = [int(x) for x in POS_RE.findall(line)]
        if positions or SEEK_RE.search(line) or "cues" in line.lower():
            events.append(
                {
                    "line": line.strip()[:240],
                    "positions": positions,
                }
            )
    flat = []
    for e in events:
        flat.extend(e["positions"])
    spans = []
    if len(flat) >= 2:
        for a, b in zip(flat, flat[1:]):
            spans.append({"from": a, "to": b, "delta": b - a})
    return {
        "event_count": len(events),
        "position_count": len(flat),
        "positions_head": flat[:40],
        "positions_unique": len(set(flat)),
        "max_jump": max((abs(s["delta"]) for s in spans), default=None),
        "events_head": events[:30],
    }


def time_seek(ss_s: float) -> tuple[int, str, int]:
    return run_ffmpeg(
        [
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "debug",
            "-y",
            "-ss",
            f"{ss_s:.3f}",
            "-i",
            str(UP),
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-f",
            "null",
            "-",
        ]
    )


def byte_seek(byte_pos: int) -> tuple[int, str, int]:
    # Jump demuxer start to known keyframe cluster byte; no -ss.
    return run_ffmpeg(
        [
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "debug",
            "-y",
            "-skip_initial_bytes",
            str(byte_pos),
            "-f",
            "matroska",
            "-i",
            str(UP),
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-f",
            "null",
            "-",
        ]
    )


def stats(vals: list[int]) -> dict:
    s = sorted(vals)
    return {
        "n": len(s),
        "min": s[0] if s else None,
        "p50": s[(len(s) - 1) // 2] if s else None,
        "max": s[-1] if s else None,
    }


def main():
    if not UP.exists():
        raise SystemExit(f"missing {UP}")

    rows = []
    # Order: for each case, alternate byte-first then ss on first rep to
    # give byte path a shot at colder cache; then more reps.
    for case in CASES:
        for i in range(N):
            # byte-offset first on even reps
            order = ("byte", "ss") if i % 2 == 0 else ("ss", "byte")
            for kind in order:
                if kind == "ss":
                    rc, err, ms = time_seek(case["ss_s"])
                    io = parse_debug_io(err)
                    row = {
                        "case": case["label"],
                        "kind": "ss_time",
                        "rep": i,
                        "target_s": case["ss_s"],
                        "target_byte": case["byte_pos"],
                        "rc": rc,
                        "ms": ms,
                        "io": io,
                        "stderr_tail": err[-1500:],
                    }
                else:
                    rc, err, ms = byte_seek(case["byte_pos"])
                    io = parse_debug_io(err)
                    row = {
                        "case": case["label"],
                        "kind": "byte_skip",
                        "rep": i,
                        "target_s": case["ss_s"],
                        "target_byte": case["byte_pos"],
                        "rc": rc,
                        "ms": ms,
                        "io": io,
                        "stderr_tail": err[-1500:],
                    }
                rows.append(row)
                print(
                    json.dumps(
                        {
                            "case": row["case"],
                            "kind": row["kind"],
                            "rep": i,
                            "ms": ms,
                            "rc": rc,
                            "io_events": io["event_count"],
                            "io_positions": io["position_count"],
                            "io_unique": io["positions_unique"],
                            "max_jump": io["max_jump"],
                        }
                    )
                )
                time.sleep(0.8)

    summary = {}
    for case in CASES:
        for kind in ("ss_time", "byte_skip"):
            vals = [r["ms"] for r in rows if r["case"] == case["label"] and r["kind"] == kind]
            summary[f"{case['label']}_{kind}"] = stats(vals)

    # Strongest IO pattern sample: first mid ss (likely colder)
    first_ss = next(r for r in rows if r["kind"] == "ss_time" and r["case"] == "mid")
    first_byte = next(r for r in rows if r["kind"] == "byte_skip" and r["case"] == "mid")

    report = {
        "stamp": "2026-08-01",
        "src": str(UP),
        "product_frame": (
            "Cold first-touch is the Gate 2 failure mode; warm clears 3s. "
            "Same cost class as scan probe, subtitle extract, keyframe map read."
        ),
        "hypotheses": {
            "scattered_small_reads": "many round trips → readahead/warmup",
            "ss_scan_without_cues": "huge sequential/index miss → byte-offset/map fixes",
        },
        "keyframes": CASES,
        "summary_ms": summary,
        "io_sample_ss_mid": first_ss["io"],
        "io_sample_byte_mid": first_byte["io"],
        "rows": [
            {k: v for k, v in r.items() if k != "stderr_tail"} for r in rows
        ],
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(report, indent=2))
    print(json.dumps({"summary_ms": summary, "wrote": str(OUT)}, indent=2))


if __name__ == "__main__":
    main()
