#!/usr/bin/env python3
"""Sample library MP4s for moov-before-mdat (faststart) vs end-moov.

Reports fraction + optional cold ffmpeg -ss cost on a few of each class.
"""
from __future__ import annotations

import json
import os
import random
import sqlite3
import struct
import subprocess
import time
from pathlib import Path

DB = Path(os.environ.get("NIGHTJAR_DB", "/Users/gmacarthur/nightjar-data/nightjar.db"))
OUT = Path(
    os.environ.get(
        "OUT",
        "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/mp4-faststart-2026-08-01.json",
    )
)
N = int(os.environ.get("N", "300"))
COLD_N = int(os.environ.get("COLD_N", "3"))  # per class for -ss timing
SS = float(os.environ.get("SS", "60"))


def classify_mp4(path: Path, scan_limit: int = 8 * 1024 * 1024) -> dict:
    """Walk top-level boxes from the start until moov or mdat (or scan_limit)."""
    size = path.stat().st_size
    with open(path, "rb") as f:
        pos = 0
        first = None
        moov_pos = None
        mdat_pos = None
        boxes = []
        while pos + 8 <= size and pos < scan_limit:
            f.seek(pos)
            hdr = f.read(8)
            if len(hdr) < 8:
                break
            box_size, typ = struct.unpack(">I4s", hdr)
            try:
                name = typ.decode("ascii")
            except UnicodeDecodeError:
                name = typ.hex()
            if box_size == 1:
                # 64-bit largesize
                largesize = struct.unpack(">Q", f.read(8))[0]
                header_len = 16
                box_size = largesize
            elif box_size == 0:
                box_size = size - pos
                header_len = 8
            else:
                header_len = 8
            if box_size < header_len:
                return {
                    "path": str(path),
                    "size": size,
                    "ok": False,
                    "error": f"bad box size {box_size} at {pos}",
                }
            if first is None:
                first = name
            boxes.append({"type": name, "pos": pos, "size": box_size})
            if name == "moov" and moov_pos is None:
                moov_pos = pos
            if name == "mdat" and mdat_pos is None:
                mdat_pos = pos
            if moov_pos is not None and mdat_pos is not None:
                break
            pos += box_size

        # If mdat seen but no moov in scan_limit, check near EOF for moov
        if mdat_pos is not None and moov_pos is None and size > scan_limit:
            # scan last 4 MiB for 'moov' box header
            tail = min(4 * 1024 * 1024, size)
            f.seek(size - tail)
            buf = f.read(tail)
            idx = 0
            while True:
                j = buf.find(b"moov", idx)
                if j < 0:
                    break
                # box type at j; size is 4 bytes before
                if j >= 4:
                    abs_pos = size - tail + j - 4
                    moov_pos = abs_pos
                    break
                idx = j + 1

    if moov_pos is None and mdat_pos is None:
        kind = "unknown"
    elif moov_pos is not None and (mdat_pos is None or moov_pos < mdat_pos):
        kind = "faststart"  # moov before mdat
    elif mdat_pos is not None and (moov_pos is None or mdat_pos < moov_pos):
        kind = "end_moov"
    else:
        kind = "unknown"

    return {
        "path": str(path),
        "size": size,
        "ok": True,
        "kind": kind,
        "first_box": first,
        "moov_pos": moov_pos,
        "mdat_pos": mdat_pos,
        "boxes_head": boxes[:12],
    }


def ffmpeg_ss_wall(path: Path, ss: float) -> dict:
    """Cold-ish open: ffmpeg -ss then quit after demux starts (null mux, -t 0.1)."""
    args = [
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-ss",
        f"{ss}",
        "-i",
        str(path),
        "-map",
        "0:v:0",
        "-c",
        "copy",
        "-f",
        "null",
        "-t",
        "0.1",
        "-",
    ]
    t0 = time.perf_counter()
    p = subprocess.run(args, capture_output=True)
    wall = int((time.perf_counter() - t0) * 1000)
    return {
        "wall_ms": wall,
        "rc": p.returncode,
        "err_tail": (p.stderr.decode("utf-8", "replace") if p.stderr else "")[-300:],
    }


def main():
    con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    total = con.execute("SELECT COUNT(*) FROM media_items").fetchone()[0]
    by_container = dict(
        con.execute(
            "SELECT COALESCE(container,'(null)'), COUNT(*) FROM media_items GROUP BY 1"
        ).fetchall()
    )
    mp4_rows = con.execute(
        """
        SELECT id, path, size_bytes, container
        FROM media_items
        WHERE lower(path) LIKE '%.mp4'
           OR lower(path) LIKE '%.m4v'
           OR lower(path) LIKE '%.mov'
        """
    ).fetchall()
    con.close()

    mp4_n = len(mp4_rows)
    sample = mp4_rows if len(mp4_rows) <= N else random.Random(20260801).sample(mp4_rows, N)

    results = []
    counts = {"faststart": 0, "end_moov": 0, "unknown": 0, "error": 0, "missing": 0}
    for mid, path, size_bytes, container in sample:
        p = Path(path)
        if not p.is_file():
            counts["missing"] += 1
            results.append({"id": mid, "path": path, "ok": False, "error": "missing"})
            continue
        try:
            r = classify_mp4(p)
        except OSError as e:
            counts["error"] += 1
            results.append({"id": mid, "path": path, "ok": False, "error": str(e)})
            continue
        r["id"] = mid
        r["db_size"] = size_bytes
        r["container"] = container
        results.append(r)
        if not r.get("ok"):
            counts["error"] += 1
        else:
            counts[r["kind"]] = counts.get(r["kind"], 0) + 1
        print(
            f"{r.get('kind','err'):10} id={mid} size={size_bytes} "
            f"moov={r.get('moov_pos')} mdat={r.get('mdat_pos')} {p.name[:60]}",
            flush=True,
        )

    classified = counts["faststart"] + counts["end_moov"]
    frac_fs = counts["faststart"] / classified if classified else None

    # Cold cost: pick COLD_N of each class (largest first — worst I/O)
    timing = {"faststart": [], "end_moov": []}
    for kind in ("faststart", "end_moov"):
        cand = [r for r in results if r.get("kind") == kind and r.get("ok")]
        cand.sort(key=lambda r: r.get("size") or 0, reverse=True)
        for r in cand[:COLD_N]:
            print(f"timing {kind} {r['path']}", flush=True)
            t = ffmpeg_ss_wall(Path(r["path"]), SS)
            timing[kind].append(
                {
                    "id": r["id"],
                    "path": r["path"],
                    "size": r["size"],
                    "ss_s": SS,
                    **t,
                }
            )
            print(f"  wall_ms={t['wall_ms']} rc={t['rc']}", flush=True)

    summary = {
        "library_total": total,
        "mp4_family_count": mp4_n,
        "mp4_family_pct": round(100.0 * mp4_n / total, 1) if total else None,
        "by_container": by_container,
        "sample_n": len(sample),
        "counts": counts,
        "faststart_fraction_of_classified": frac_fs,
        "faststart_pct": round(100.0 * frac_fs, 1) if frac_fs is not None else None,
        "cold_ss_timing": timing,
        "note": (
            "faststart = moov box before mdat when walking from file start. "
            "end_moov = mdat first (moov later / near EOF). "
            "Cold timing is ffmpeg -ss N -i … -c copy -f null -t 0.1 (no purge)."
        ),
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps({"summary": summary, "rows": results}, indent=2))
    print(json.dumps(summary, indent=2), flush=True)
    print("wrote", OUT, flush=True)


if __name__ == "__main__":
    main()
