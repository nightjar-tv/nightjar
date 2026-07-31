#!/usr/bin/env python3
"""Step 1b: library bitrate shape from the dogfood index.

Bitrate is not a DB column. On this corpus ffprobe format.bit_rate equals
size_bytes×8/duration_ms (ratio 1.000 on a 30-file sample); FFmpeg derives
the same number when the container has no declared rate. This script uses
that identity so the distribution is reproducible without a 10+ hour NAS
walk.

Usage:
  python3 scripts/t1b_bitrate_shape.py [/path/to/nightjar.db]
"""

from __future__ import annotations

import json
import math
import os
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path


def default_db() -> Path:
    env = os.environ.get("NIGHTJAR_DATA_DIR")
    if env:
        return Path(env) / "nightjar.db"
    home = Path.home() / "nightjar-data" / "nightjar.db"
    return home if home.is_file() else Path("data") / "nightjar.db"


def pct(sorted_vals: list[float], p: float) -> float:
    n = len(sorted_vals)
    i = min(n - 1, max(0, math.ceil(p / 100.0 * n) - 1))
    return sorted_vals[i]


def source_tag(path: str) -> str:
    p = path
    if "Remux" in p:
        return "Remux"
    if any(x in p for x in ("Bluray", "BluRay", "Blu-ray")):
        return "Bluray"
    if any(x in p for x in ("WEBDL", "WEB-DL", "WEBRip", "WEB")):
        return "WEB"
    if "HDTV" in p:
        return "HDTV"
    if "DVD" in p:
        return "DVD"
    return "other"


def res_tier(height: int | None) -> str:
    h = height or 0
    if h >= 2160:
        return "2160p+"
    if h >= 1440:
        return "1440p"
    if h >= 1080:
        return "1080p"
    if h >= 720:
        return "720p"
    if h >= 480:
        return "480p"
    if h > 0:
        return "<480p"
    return "unknown"


def group_stats(vals: list[float], n_lib: int) -> dict:
    vs = sorted(vals)
    n = len(vs)
    return {
        "n": n,
        "pct_lib": round(100.0 * n / n_lib, 2),
        "p50": round(pct(vs, 50), 3),
        "p90": round(pct(vs, 90), 3),
        "p99": round(pct(vs, 99), 3),
        "max": round(vs[-1], 3),
        "exceed_8": round(100.0 * sum(1 for x in vs if x > 8) / n, 2),
        "exceed_15": round(100.0 * sum(1 for x in vs if x > 15) / n, 2),
        "exceed_25": round(100.0 * sum(1 for x in vs if x > 25) / n, 2),
    }


def main() -> int:
    db = Path(sys.argv[1]) if len(sys.argv) > 1 else default_db()
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = con.execute(
        """
        SELECT path, size_bytes, duration_ms, height
        FROM media_items
        WHERE probe_status='probed' AND duration_ms > 0 AND size_bytes > 0
          AND path NOT LIKE '%/testdata/%'
        """
    ).fetchall()
    con.close()

    mbps_list: list[float] = []
    by_tier: dict[str, list[float]] = defaultdict(list)
    by_src: dict[str, list[float]] = defaultdict(list)
    for path, size, dur, height in rows:
        mbps = size * 8.0 * 1000.0 / dur / 1e6
        mbps_list.append(mbps)
        by_tier[res_tier(height)].append(mbps)
        by_src[source_tag(path)].append(mbps)

    n = len(mbps_list)
    sorted_m = sorted(mbps_list)
    edges = [0, 1, 2, 4, 6, 8, 10, 12, 15, 20, 25, 30, 40, 50, 60, 80, 100, 200]
    hist = []
    for i, lo in enumerate(edges):
        hi = edges[i + 1] if i + 1 < len(edges) else 1e9
        c = sum(1 for x in mbps_list if lo <= x < hi)
        label = f"{lo:g}–{hi:g}" if hi < 1e8 else f"≥{lo:g}"
        hist.append({"bucket_mbps": label, "n": c, "pct": round(100.0 * c / n, 2)})

    ceilings = [4, 8, 15, 25]
    exceed = [
        {
            "ceiling_mbps": c,
            "exceed_n": sum(1 for x in mbps_list if x > c),
            "exceed_pct": round(100.0 * sum(1 for x in mbps_list if x > c) / n, 2),
        }
        for c in ceilings
    ]

    summary = {
        "db": str(db),
        "n": n,
        "unit": "Mbps",
        "method": "size_bytes×8/duration_ms; equals ffprobe format.bit_rate on this corpus (n=30 ratio 1.000)",
        "excluded": "paths containing /testdata/",
        "overall": {
            "min": round(sorted_m[0], 3),
            "p50": round(pct(sorted_m, 50), 3),
            "p90": round(pct(sorted_m, 90), 3),
            "p99": round(pct(sorted_m, 99), 3),
            "max": round(sorted_m[-1], 3),
        },
        "histogram_mbps": hist,
        "exceed_ceiling": exceed,
        "by_resolution_tier": {
            k: group_stats(v, n)
            for k, v in sorted(by_tier.items(), key=lambda kv: -len(kv[1]))
        },
        "by_source_tag": {
            k: group_stats(v, n)
            for k, v in sorted(by_src.items(), key=lambda kv: -len(kv[1]))
        },
    }

    out = Path("notes/client-arch/bitrate-shape-2026-07-31.json")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(summary, indent=2))
    print(json.dumps({k: summary[k] for k in ("n", "overall", "exceed_ceiling")}, indent=2))
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
