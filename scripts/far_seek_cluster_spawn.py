#!/usr/bin/env python3
"""Prove Cluster-offset spawn: synthetic header+Cluster → production-like HLS.

Measures cold wall (NAS preads + encode to first segment) and first-segment
PTS / sidx against the requested title time. Also copy-mode and a note/probe
hook for MP4 (different container map).
"""
from __future__ import annotations

import json
import os
import shutil
import struct
import subprocess
import tempfile
import time
from pathlib import Path

UP = Path("/Volumes/media/Movies/Up (2009)/Up (2009) Bluray-1080p.mkv")
# h264 MP4 for the 13% case (different map shape — stss/moov)
MP4 = Path(
    "/Volumes/media/Movies/28 Days Later (2002)/28 Days Later (2002) Bluray-1080p.mp4"
)
OUT = Path(
    os.environ.get(
        "OUT",
        "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/far-seek-cluster-spawn-2026-08-01.json",
    )
)
CLUSTER_ID = bytes([0x1F, 0x43, 0xB6, 0x75])
SEG_MS = 2000
LEAD_SECS = 16  # enough for first land segment
# mid / far keyframes from prior notes
CASES = [
    {"label": "mid", "ss_s": 44.461, "pkt_pos": 66_136_568},
    {"label": "far", "ss_s": 2177.090, "pkt_pos": 2_920_900_180},
]
N = int(os.environ.get("N", "3"))
GATE_MS = 3000


def find_first_cluster(path: Path, limit: int = 16 * 1024 * 1024) -> int:
    with open(path, "rb") as f:
        buf = f.read(limit)
    i = buf.find(CLUSTER_ID)
    if i < 0:
        raise RuntimeError("no Cluster in header scan")
    return i


def find_cluster_before(path: Path, pos: int, back: int = 16_000_000) -> int:
    start = max(0, pos - back)
    with open(path, "rb") as f:
        f.seek(start)
        buf = f.read(pos - start + 16)
    abs_off = None
    idx = 0
    while True:
        j = buf.find(CLUSTER_ID, idx)
        if j < 0:
            break
        abs_off = start + j
        idx = j + 1
    if abs_off is None:
        raise RuntimeError(f"no Cluster before {pos}")
    return abs_off


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


def build_synthetic(src: Path, header_end: int, cluster_pos: int, body_bytes: int) -> tuple[Path, dict]:
    """Write header[0:header_end] + body[cluster_pos:…] to a local temp mkv."""
    t0 = time.perf_counter()
    with open(src, "rb") as f:
        header = f.read(header_end)
        f.seek(cluster_pos)
        body = f.read(body_bytes)
    read_ms = int((time.perf_counter() - t0) * 1000)
    out = Path(tempfile.mkstemp(prefix="nj_synth_", suffix=".mkv")[1])
    t1 = time.perf_counter()
    with open(out, "wb") as f:
        f.write(header)
        f.write(body)
    write_ms = int((time.perf_counter() - t1) * 1000)
    return out, {
        "read_ms": read_ms,
        "write_ms": write_ms,
        "header_bytes": len(header),
        "body_bytes": len(body),
        "synth_bytes": len(header) + len(body),
        "cluster_pos": cluster_pos,
        "header_end": header_end,
    }


def parse_sidx_ept(path: Path) -> float | None:
    """Return earliest_presentation_time from first sidx, in seconds (timescale)."""
    data = path.read_bytes()
    # find 'sidx'
    idx = data.find(b"sidx")
    if idx < 4:
        return None
    # box starts 4 bytes before type
    start = idx - 4
    if start < 0:
        return None
    size = struct.unpack(">I", data[start : start + 4])[0]
    if size < 28 or start + size > len(data):
        # largesize not handled; try version parse at fixed layout
        pass
    version = data[idx + 4]
    # sidx after type(4)+version(1)+flags(3)+ref_id(4)+timescale(4)
    timescale = struct.unpack(">I", data[idx + 12 : idx + 16])[0]
    if timescale == 0:
        return None
    if version == 0:
        ept = struct.unpack(">I", data[idx + 16 : idx + 20])[0]
    else:
        ept = struct.unpack(">Q", data[idx + 16 : idx + 24])[0]
    return ept / timescale


def spawn_hls(
    synth: Path,
    mode: str,
    encoder: str,
    requested_s: float,
    *,
    use_ts_offset: bool,
) -> dict:
    """Production-like HLS spawn. No -ss (synthetic already starts at land)."""
    out = Path(tempfile.mkdtemp(prefix="nj_spawn_"))
    progress = out / "progress.txt"
    start_number = int(requested_s * 1000 // SEG_MS)
    force_kf = f"expr:gte(t,n_forced*{SEG_MS/1000})"
    args = [
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-i",
        str(synth),
    ]
    if use_ts_offset and requested_s > 0:
        args += ["-output_ts_offset", f"{requested_s:.3f}"]
    args += ["-progress", str(progress), "-map", "0:v:0", "-map", "0:a:0?"]
    if mode == "copy":
        args += ["-c", "copy"]
    else:
        args += [
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
        ]
    args += [
        "-f",
        "hls",
        "-hls_time",
        str(SEG_MS / 1000),
        "-hls_segment_type",
        "fmp4",
        "-hls_flags",
        "independent_segments",
        "-start_number",
        str(start_number),
        "-hls_segment_filename",
        str(out / f"seg_%05d.m4s"),
        "-t",
        str(LEAD_SECS),
        str(out / "index.m3u8"),
    ]
    t0 = time.perf_counter()
    proc = subprocess.Popen(args, stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
    first_seg_ms = None
    first_seg = None
    init_ms = None
    deadline = t0 + 90
    while time.perf_counter() < deadline:
        now = int((time.perf_counter() - t0) * 1000)
        if init_ms is None and (out / "init.mp4").exists() and (out / "init.mp4").stat().st_size > 0:
            init_ms = now
        segs = sorted(out.glob("seg_*.m4s"))
        if segs and first_seg is None:
            if segs[0].stat().st_size > 0:
                first_seg = segs[0]
                first_seg_ms = now
                break
        if proc.poll() is not None and first_seg is None:
            break
        time.sleep(0.05)
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    err = ""
    if proc.stderr:
        try:
            err = proc.stderr.read().decode("utf-8", "replace")[-2000:]
        except Exception:
            pass

    sidx_ept = parse_sidx_ept(first_seg) if first_seg else None
    # Also probe first video packet pts from the segment if possible
    pkt_pts = None
    if first_seg:
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
                str(first_seg),
            ],
            capture_output=True,
            text=True,
        )
        line = (pr.stdout or "").strip().splitlines()
        if line:
            try:
                pkt_pts = float(line[0].split(",")[0])
            except ValueError:
                pkt_pts = None

    return {
        "encode_ms": first_seg_ms,
        "init_ms": init_ms,
        "first_seg": first_seg.name if first_seg else None,
        "sidx_ept_s": sidx_ept,
        "pkt_pts_s": pkt_pts,
        "requested_s": requested_s,
        "use_ts_offset": use_ts_offset,
        "rc": proc.returncode,
        "err_tail": err,
        "out_dir": str(out),
    }


def baseline_ss_spawn(src: Path, requested_s: float, mode: str, encoder: str) -> dict:
    """Control: production -ss on the real NAS file (no synthetic)."""
    out = Path(tempfile.mkdtemp(prefix="nj_base_"))
    start_number = int(requested_s * 1000 // SEG_MS)
    force_kf = f"expr:gte(t,n_forced*{SEG_MS/1000})"
    args = [
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-ss",
        f"{requested_s:.3f}",
        "-i",
        str(src),
        "-output_ts_offset",
        f"{requested_s:.3f}",
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
            encoder,
            "-pix_fmt",
            "yuv420p",
            "-map_metadata",
            "-1",
            "-vf",
            "sidedata=delete,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709",
            "-force_key_frames",
            force_kf,
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
        str(SEG_MS / 1000),
        "-hls_segment_type",
        "fmp4",
        "-hls_flags",
        "independent_segments",
        "-start_number",
        str(start_number),
        "-hls_segment_filename",
        str(out / "seg_%05d.m4s"),
        "-t",
        str(LEAD_SECS),
        str(out / "index.m3u8"),
    ]
    t0 = time.perf_counter()
    proc = subprocess.Popen(args, stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
    first_seg_ms = None
    first_seg = None
    deadline = t0 + 90
    while time.perf_counter() < deadline:
        segs = sorted(out.glob("seg_*.m4s"))
        if segs and segs[0].stat().st_size > 0:
            first_seg = segs[0]
            first_seg_ms = int((time.perf_counter() - t0) * 1000)
            break
        if proc.poll() is not None:
            break
        time.sleep(0.05)
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    sidx = parse_sidx_ept(first_seg) if first_seg else None
    shutil.rmtree(out, ignore_errors=True)
    return {
        "encode_ms": first_seg_ms,
        "first_seg": first_seg.name if first_seg else None,
        "sidx_ept_s": sidx,
        "requested_s": requested_s,
    }


def pts_ok(requested_s: float, sidx: float | None, pkt: float | None, tol_s: float = 0.5) -> dict:
    """ADR-0020 lesson: advertised land must match produced time."""
    checks = {}
    if sidx is not None:
        checks["sidx_delta_s"] = sidx - requested_s
        checks["sidx_ok"] = abs(sidx - requested_s) <= tol_s
    if pkt is not None:
        # packet pts in segment may be 0-based (tfdt local) — record only
        checks["pkt_pts_s"] = pkt
    return checks


def main():
    if not UP.exists():
        raise SystemExit(f"missing {UP}")
    enc = pick_encoder()
    header_end = find_first_cluster(UP)
    # ~Bluray bitrate: 16s ≈ 40–80MB; take 64MB of clusters
    body_bytes = 64 * 1024 * 1024
    print(json.dumps({"encoder": enc, "header_end": header_end, "body_bytes": body_bytes}))

    rows = []
    for case in CASES:
        cluster = find_cluster_before(UP, case["pkt_pos"])
        case_info = {**case, "cluster_pos": cluster}
        print(json.dumps({"case": case_info}))

        for mode in ("transcode", "copy"):
            # Discover which ts_offset mode tells the truth on first synth build
            for rep in range(N):
                wall0 = time.perf_counter()
                synth, io = build_synthetic(UP, header_end, cluster, body_bytes)
                built = time.perf_counter()
                # Try WITHOUT ts_offset first on rep0 to see native Cluster PTS;
                # production wants title-absolute — pick based on measurement.
                use_offset = True  # ADR-0020 production always sets it on mid-start
                hls = spawn_hls(
                    synth, mode, enc, case["ss_s"], use_ts_offset=use_offset
                )
                wall_ms = int((time.perf_counter() - wall0) * 1000)
                build_ms = int((built - wall0) * 1000)
                try:
                    synth.unlink()
                except OSError:
                    pass
                shutil.rmtree(hls["out_dir"], ignore_errors=True)

                check = pts_ok(case["ss_s"], hls.get("sidx_ept_s"), hls.get("pkt_pts_s"))
                row = {
                    "container": "matroska",
                    "label": case["label"],
                    "mode": mode,
                    "rep": rep,
                    "requested_s": case["ss_s"],
                    "cluster_pos": cluster,
                    "io": io,
                    "build_ms": build_ms,
                    "wall_ms": wall_ms,
                    "land_ms": hls.get("encode_ms"),
                    "under_3s_wall": wall_ms < GATE_MS if hls.get("encode_ms") else False,
                    "under_3s_land": (hls.get("encode_ms") or 99999) < GATE_MS,
                    "hls": {k: v for k, v in hls.items() if k not in ("out_dir", "err_tail")},
                    "err_tail": hls.get("err_tail"),
                    "pts": check,
                }
                rows.append(row)
                print(
                    json.dumps(
                        {
                            "label": case["label"],
                            "mode": mode,
                            "rep": rep,
                            "wall_ms": wall_ms,
                            "build_ms": build_ms,
                            "land_ms": hls.get("encode_ms"),
                            "sidx_ept_s": hls.get("sidx_ept_s"),
                            "pts": check,
                            "first_seg": hls.get("first_seg"),
                        }
                    )
                )
                time.sleep(0.5)

            # One baseline -ss control per mode/label (warm-ish OK for contrast)
            base = baseline_ss_spawn(UP, case["ss_s"], mode, enc)
            rows.append(
                {
                    "container": "matroska",
                    "label": case["label"],
                    "mode": mode,
                    "kind": "baseline_ss",
                    "requested_s": case["ss_s"],
                    "baseline": base,
                }
            )
            print(json.dumps({"kind": "baseline_ss", "label": case["label"], "mode": mode, **base}))

    # MP4 note + light AVIO probe (not Cluster — stss/moov)
    mp4_note = {
        "path": str(MP4),
        "exists": MP4.exists(),
        "approach": (
            "MP4 map is moov/stss sample byte offsets, not Matroska Clusters. "
            "Synthetic header+mdat-slice is a different construction; not claimed "
            "by this MKV spawn proof. Record as separate map shape in the map ADR."
        ),
    }
    if MP4.exists():
        t0 = time.perf_counter()
        p = subprocess.run(
            [
                "ffmpeg",
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "verbose",
                "-y",
                "-ss",
                "60",
                "-i",
                str(MP4),
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-f",
                "null",
                "-",
            ],
            capture_output=True,
            text=True,
            timeout=120,
        )
        ms = int((time.perf_counter() - t0) * 1000)
        avio = next(
            (
                ln.strip()
                for ln in (p.stderr or "").splitlines()
                if "bytes read" in ln and "seeks" in ln
            ),
            None,
        )
        mp4_note["ss_60_ms"] = ms
        mp4_note["avio"] = avio

    def summarize(mode: str, label: str) -> dict:
        vals = [
            r
            for r in rows
            if r.get("mode") == mode
            and r.get("label") == label
            and "wall_ms" in r
        ]
        if not vals:
            return {}
        walls = sorted(r["wall_ms"] for r in vals)
        lands = sorted(r["land_ms"] for r in vals if r.get("land_ms") is not None)
        sidxs = [r["hls"].get("sidx_ept_s") for r in vals]
        return {
            "n": len(vals),
            "wall_p50": walls[len(walls) // 2],
            "wall_min": walls[0],
            "wall_max": walls[-1],
            "land_p50": lands[len(lands) // 2] if lands else None,
            "under_3s_wall": sum(1 for r in vals if r.get("under_3s_wall")),
            "sidx_samples": sidxs,
            "requested_s": vals[0]["requested_s"],
        }

    summary = {
        "stamp": "2026-08-01",
        "gate_ms": GATE_MS,
        "encoder": enc,
        "mechanism": (
            "synthetic mkv = bytes[0:first_Cluster) + bytes[land_Cluster:land+64MiB]; "
            "ffmpeg -i synth without -ss; -output_ts_offset = requested title time"
        ),
        "by": {
            f"{label}_{mode}": summarize(mode, label)
            for label in ("mid", "far")
            for mode in ("transcode", "copy")
        },
        "mp4": mp4_note,
        "rows": rows,
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(summary, indent=2))
    print(json.dumps({"summary_by": summary["by"], "mp4": mp4_note, "wrote": str(OUT)}, indent=2))


if __name__ == "__main__":
    main()
