#!/usr/bin/env python3
"""Stratified T4 / Part A sample for engine bake-off.

Selects MPV_V0 directPlay titles from the dogfood DB, oversampling the tail
(VC-1, MPEG-4, unusual audio, damaged-class paths).

Usage:
  python3 apps/engine-bakeoff/tool/stratified_sample.py [/path/to/nightjar.db]
"""

from __future__ import annotations

import json
import random
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts"))
from t1_profile_counts import MPV_V0, decide  # noqa: E402

OUT = ROOT / "notes" / "client-arch" / "bakeoff-sample.json"
SEED = 20260731


def strata_key(video_codec: str | None, audio_codec: str | None, path: str) -> str:
    vc = (video_codec or "unknown").lower()
    ac = (audio_codec or "unknown").lower()
    if "8519" in path or "8512" in path:
        return "damaged_class"
    if vc in {"vc1", "wmv3"}:
        return "vc1"
    if vc in {"mpeg4", "msmpeg4v3", "xvid", "divx"}:
        return "mpeg4"
    if vc in {"mpeg2video", "mpeg1video"}:
        return "mpeg2"
    if ac in {"dts", "truehd", "mlp", "pcm_bluray", "pcm_s24le"}:
        return "heavy_audio"
    if vc == "av1":
        return "av1"
    if vc == "vp9":
        return "vp9"
    if vc in {"hevc", "h265", "hev1", "hvc1"}:
        return "hevc"
    if vc in {"h264", "avc", "avc1"}:
        return "h264"
    return f"other:{vc}"


TARGETS = {
    "vc1": 40,
    "mpeg4": 40,
    "mpeg2": 30,
    "heavy_audio": 50,
    "damaged_class": 20,
    "av1": 30,
    "vp9": 30,
    "hevc": 40,
    "h264": 40,
}


def main() -> None:
    db = Path(sys.argv[1]) if len(sys.argv) > 1 else Path.home() / "nightjar-data" / "nightjar.db"
    rng = random.Random(SEED)
    by_stratum: dict[str, list[dict]] = defaultdict(list)
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    cur = conn.execute(
        """
        SELECT id, path, container, video_codec, audio_codec, audio_channels,
               duration_ms, size_bytes, scan_error, probe_status
        FROM media_items
        WHERE path NOT LIKE '%/testdata/%'
        """
    )
    for r in cur:
        method = decide(
            r["path"] or "",
            r["container"],
            r["video_codec"],
            r["audio_codec"],
            r["audio_channels"],
            r["scan_error"],
            r["probe_status"] or "probed",
            MPV_V0,
        )
        if method != "directPlay":
            continue
        key = strata_key(r["video_codec"], r["audio_codec"], r["path"] or "")
        bitrate_bps = None
        if r["duration_ms"] and r["duration_ms"] > 0 and r["size_bytes"]:
            bitrate_bps = int(r["size_bytes"] * 8 / (r["duration_ms"] / 1000.0))
        by_stratum[key].append(
            {
                "id": r["id"],
                "path": r["path"],
                "container": r["container"],
                "video_codec": r["video_codec"],
                "audio_codec": r["audio_codec"],
                "audio_channels": r["audio_channels"],
                "duration_ms": r["duration_ms"],
                "size_bytes": r["size_bytes"],
                "bitrate_bps_est": bitrate_bps,
                "stratum": key,
            }
        )
    conn.close()

    sample: list[dict] = []
    stratum_counts: dict[str, dict] = {}
    for key, target in TARGETS.items():
        pool = by_stratum.get(key, [])
        take = min(target, len(pool))
        chosen = rng.sample(pool, take) if take else []
        sample.extend(chosen)
        stratum_counts[key] = {"available": len(pool), "drawn": take, "target": target}

    other_drawn = 0
    for k, pool in by_stratum.items():
        if not k.startswith("other:"):
            continue
        take = min(5, len(pool))
        if take:
            sample.extend(rng.sample(pool, take))
            other_drawn += take
            stratum_counts[k] = {"available": len(pool), "drawn": take, "target": 5}

    latency_pool = [
        t
        for t in sample
        if t.get("duration_ms")
        and t["duration_ms"] >= 600_000
        and t["stratum"] in {"h264", "hevc", "heavy_audio"}
    ]
    latency_ids = [t["id"] for t in rng.sample(latency_pool, min(12, len(latency_pool)))]

    part_b_pool = [
        t
        for t in by_stratum.get("hevc", []) + by_stratum.get("heavy_audio", [])
        if t.get("duration_ms") and t["duration_ms"] >= 300_000
    ]
    part_b = rng.sample(part_b_pool, min(15, len(part_b_pool)))

    payload = {
        "seed": SEED,
        "db": str(db),
        "method": "stratified_oversample_tail",
        "note": (
            "MPV_V0 directPlay only. Tail strata oversampled vs library share. "
            "Part A stream URLs constructed as /api/v0/items/{id}/stream — "
            "playback-info is hardcoded BROWSER_V0 and will not return directPlay for MKV."
        ),
        "stratum_counts": stratum_counts,
        "t4_sample_n": len(sample),
        "t4_sample": sample,
        "latency_item_ids": latency_ids,
        "part_b_candidates": part_b,
        "other_drawn": other_drawn,
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(payload, indent=2))
    print(f"wrote {OUT} t4_n={len(sample)} latency={len(latency_ids)} part_b={len(part_b)}")
    for k, v in sorted(stratum_counts.items()):
        print(f"  {k}: drawn {v['drawn']}/{v['available']} (target {v['target']})")


if __name__ == "__main__":
    main()
