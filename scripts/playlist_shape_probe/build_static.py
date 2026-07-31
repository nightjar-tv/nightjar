#!/usr/bin/env python3
"""Build static/ time-keyed media + three playlist shapes from work/run_*."""
from __future__ import annotations

import json
import re
import shutil
import struct
from pathlib import Path

PROBE = Path(__file__).resolve().parent
WORK = PROBE / "work"
STATIC = PROBE / "static"
MEDIA = STATIC / "media"
MEDIA.mkdir(parents=True, exist_ok=True)
TITLE_DURATION_S = 1325.575


def boxes(data, start=0, end=None):
    if end is None:
        end = len(data)
    off = start
    while off + 8 <= end:
        size = struct.unpack(">I", data[off : off + 4])[0]
        typ = data[off + 4 : off + 8]
        if size == 1:
            size = struct.unpack(">Q", data[off + 8 : off + 16])[0]
            hdr = 16
        elif size == 0:
            size = end - off
            hdr = 8
        else:
            hdr = 8
        if size < hdr or off + size > end:
            break
        yield typ, off, hdr, size
        off += size


def sidx_video_earliest_ms(seg: bytes) -> float:
    for typ, off, hdr, size in boxes(seg):
        if typ != b"sidx":
            continue
        body = seg[off + hdr : off + size]
        ver = body[0]
        if struct.unpack(">I", body[4:8])[0] != 1:
            continue
        timescale = struct.unpack(">I", body[8:12])[0]
        pos = 12
        earliest = struct.unpack(
            ">I" if ver == 0 else ">Q", body[pos : pos + (4 if ver == 0 else 8)]
        )[0]
        return earliest * 1000 / timescale
    raise SystemExit("no video sidx")


def parse_ffmpeg_index(run_dir: Path) -> dict:
    text = (run_dir / "index.m3u8").read_text()
    target = int(re.search(r"#EXT-X-TARGETDURATION:(\d+)", text).group(1))
    entries = re.findall(r"#EXTINF:([0-9.]+),\s*\n(seg\d+\.m4s)", text)
    segs = []
    for extinf, name in entries:
        earliest = sidx_video_earliest_ms((run_dir / name).read_bytes())
        start_ms = int(round(earliest))
        segs.append(
            {
                "ffmpeg_name": name,
                "uri": f"seg_{start_ms:011d}.m4s",
                "start_ms": start_ms,
                "extinf_s": float(extinf),
                "sidx_ms": earliest,
                "gate_ok": abs(start_ms - round(earliest)) <= 1,
            }
        )
    return {"target": target, "segs": segs}


def playlist_body(runs: dict, run_name: str, playlist_type: str, endlist: bool) -> str:
    r = runs[run_name]
    segs = r["segs"]
    target = max(r["target"], int(max(s["extinf_s"] for s in segs)) + 1)
    lines = [
        "#EXTM3U",
        "#EXT-X-VERSION:7",
        f"#EXT-X-TARGETDURATION:{target}",
        f"#EXT-X-PLAYLIST-TYPE:{playlist_type}",
        "#EXT-X-MEDIA-SEQUENCE:0",
        "#EXT-X-INDEPENDENT-SEGMENTS",
        f'#EXT-X-MAP:URI="media/{r["init_uri"]}"',
    ]
    start_s = segs[0]["start_ms"] / 1000.0
    if start_s > 0.5:
        lines.append(f"#EXT-X-START:TIME-OFFSET={start_s:.3f},PRECISE=YES")
    for s in segs:
        lines.append(f"#EXTINF:{s['extinf_s']:.6f},")
        lines.append(f"media/{s['uri']}")
    if endlist:
        lines.append("#EXT-X-ENDLIST")
    return "\n".join(lines) + "\n"


def main() -> None:
    runs: dict = {}
    for name in ("run_a", "run_b"):
        runs[name] = parse_ffmpeg_index(WORK / name)
        init_uri = f"init_{name}.mp4"
        shutil.copy2(WORK / name / "init.mp4", MEDIA / init_uri)
        runs[name]["init_uri"] = init_uri
        for s in runs[name]["segs"]:
            shutil.copy2(WORK / name / s["ffmpeg_name"], MEDIA / s["uri"])

    (STATIC / "map.json").write_text(
        json.dumps({"title_duration_s": TITLE_DURATION_S, "runs": runs}, indent=2)
    )
    (STATIC / "shape_a_event_region_b.m3u8").write_text(
        playlist_body(runs, "run_b", "EVENT", False)
    )
    (STATIC / "shape_b_land_a.m3u8").write_text(playlist_body(runs, "run_a", "EVENT", False))
    (STATIC / "shape_b_land_b.m3u8").write_text(playlist_body(runs, "run_b", "EVENT", False))
    (STATIC / "shape_c_land_a.m3u8").write_text(playlist_body(runs, "run_a", "VOD", True))
    (STATIC / "shape_c_land_b.m3u8").write_text(playlist_body(runs, "run_b", "VOD", True))
    (STATIC / "shape_a_event.m3u8").write_text((STATIC / "shape_b_land_a.m3u8").read_text())
    assert all(s["gate_ok"] for r in runs.values() for s in r["segs"])
    print(
        "ok",
        f"a={runs['run_a']['segs'][0]['start_ms']}..{runs['run_a']['segs'][-1]['start_ms']}",
        f"b={runs['run_b']['segs'][0]['start_ms']}..{runs['run_b']['segs'][-1]['start_ms']}",
    )


if __name__ == "__main__":
    main()
