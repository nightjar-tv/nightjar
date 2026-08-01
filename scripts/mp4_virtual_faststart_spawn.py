#!/usr/bin/env python3
"""MP4 session-start mechanism proof: virtual faststart over HTTP Range.

End-moov files force FFmpeg to hunt the tail for moov (~19 seeks / ~12 s on
the dogfood point). A Matroska-style header||Cluster splice is NOT valid for
MP4: sample offsets in moov are absolute in the original file.

Mechanism under test (candidate to lock in ADR-0023):
  Present a virtual file [ftyp][moov'][mdat] where moov' has stco/co64
  rewritten (classic qt-faststart delta), mdat Range-mapped to the original
  mdat. No media temp copy. FFmpeg opens the URL with -ss at the snapped
  land (map PTS) — index seek into mdat, not a moov hunt.

Also records naive splice (header + mdat[land:]) without rewrite — expected
fail — so the ADR can cite why Matroska splice must not be reused.

Usage:
  sudo purge   # optional cold
  PYTHONUNBUFFERED=1 python3 scripts/mp4_virtual_faststart_spawn.py
"""
from __future__ import annotations

import json
import shutil
import struct
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from far_seek_cluster_spawn import parse_sidx_ept, pick_encoder  # noqa: E402

# End-moov WEBRip ~727 MiB — small enough to iterate; layout verified mdat-then-moov
SRC = Path(
    "/Volumes/media/TV Shows/Greys Anatomy/Season 6/"
    "Grey's Anatomy - 6x14 - Valentine's Day Massacre - WEBRip-1080p.mp4"
)
# Prior dogfood end-moov class (large); optional second pass via env
SS_S = float(__import__("os").environ.get("SS", "60"))
GATE_MS = 3000
LEAD_SECS = 16
OUT = Path(
    "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/"
    "mp4-virtual-faststart-spawn-2026-08-01.json"
)
CHUNK = 256 * 1024


def read_boxes(path: Path) -> list[dict]:
    size = path.stat().st_size
    boxes = []
    pos = 0
    with open(path, "rb") as f:
        while pos + 8 <= size:
            f.seek(pos)
            hdr = f.read(8)
            box_size, typ = struct.unpack(">I4s", hdr)
            header_len = 8
            if box_size == 1:
                box_size = struct.unpack(">Q", f.read(8))[0]
                header_len = 16
            elif box_size == 0:
                box_size = size - pos
            if box_size < header_len:
                break
            boxes.append(
                {
                    "type": typ.decode("latin1"),
                    "pos": pos,
                    "size": box_size,
                    "header_len": header_len,
                }
            )
            pos += box_size
            if pos >= size:
                break
    return boxes


def find_box(boxes: list[dict], name: str) -> dict:
    for b in boxes:
        if b["type"] == name:
            return b
    raise RuntimeError(f"no {name} box")


def rewrite_chunk_offsets(moov: bytearray, delta: int) -> int:
    """Add delta to every stco/co64 entry. Returns number of offsets touched."""
    n = 0
    i = 0
    while i + 8 <= len(moov):
        # scan for stco/co64 box headers inside moov
        if moov[i : i + 4] in (b"stco", b"co64") and i >= 4:
            box_start = i - 4
            box_size = struct.unpack(">I", moov[box_start : box_start + 4])[0]
            typ = bytes(moov[i : i + 4])
            if box_size < 16 or box_start + box_size > len(moov):
                i += 1
                continue
            # fullbox: ver+flags at i+4
            ver = moov[i + 4]
            entry_count = struct.unpack(">I", moov[i + 8 : i + 12])[0]
            off = i + 12
            if typ == b"stco":
                for _ in range(entry_count):
                    if off + 4 > box_start + box_size:
                        break
                    val = struct.unpack(">I", moov[off : off + 4])[0]
                    moov[off : off + 4] = struct.pack(">I", (val + delta) & 0xFFFFFFFF)
                    off += 4
                    n += 1
            else:  # co64
                for _ in range(entry_count):
                    if off + 8 > box_start + box_size:
                        break
                    val = struct.unpack(">Q", moov[off : off + 8])[0]
                    moov[off : off + 8] = struct.pack(">Q", val + delta)
                    off += 8
                    n += 1
            i = box_start + box_size
            continue
        i += 1
    return n


def build_virtual_faststart(path: Path) -> dict:
    boxes = read_boxes(path)
    ftyp = find_box(boxes, "ftyp")
    mdat = find_box(boxes, "mdat")
    moov = find_box(boxes, "moov")
    # Optional free/wide between ftyp and mdat — keep prefix through mdat-1
    prefix_end = mdat["pos"]  # bytes [0, prefix_end) before mdat
    with open(path, "rb") as f:
        f.seek(0)
        prefix = f.read(prefix_end)
        f.seek(moov["pos"])
        moov_bytes = bytearray(f.read(moov["size"]))
    if moov["pos"] < mdat["pos"]:
        kind = "already_faststart"
        # identity virtual file
        return {
            "kind": kind,
            "prefix": prefix,
            "moov": bytes(moov_bytes),
            "mdat_pos": mdat["pos"],
            "mdat_size": mdat["size"],
            "delta": 0,
            "offsets_rewritten": 0,
            "src_size": path.stat().st_size,
        }
    # end-moov: new layout [prefix][moov'][mdat]
    delta = len(moov_bytes)  # mdat shifts right by moov size
    n = rewrite_chunk_offsets(moov_bytes, delta)
    return {
        "kind": "end_moov_virtual_faststart",
        "prefix": prefix,
        "moov": bytes(moov_bytes),
        "mdat_pos": mdat["pos"],
        "mdat_size": mdat["size"],
        "delta": delta,
        "offsets_rewritten": n,
        "src_size": path.stat().st_size,
    }


class VirtualFaststartHandler(BaseHTTPRequestHandler):
    """Virtual [prefix][moov'][mdat] with Range → original mdat pread."""

    prefix: bytes = b""
    moov: bytes = b""
    src_path: str = ""
    mdat_pos: int = 0
    mdat_size: int = 0
    bytes_served: int = 0
    range_hits: int = 0

    @property
    def head(self) -> bytes:
        return self.prefix + self.moov

    @property
    def total(self) -> int:
        return len(self.head) + self.mdat_size

    def log_message(self, fmt, *args):  # noqa: A003
        pass

    def _stream(self, start: int, end: int) -> int:
        head = self.head
        hlen = len(head)
        written = 0
        pos = start
        with open(self.src_path, "rb") as f:
            while pos <= end:
                n = min(CHUNK, end - pos + 1)
                if pos < hlen:
                    take = min(n, hlen - pos)
                    self.wfile.write(head[pos : pos + take])
                    written += take
                    pos += take
                else:
                    file_off = self.mdat_pos + (pos - hlen)
                    f.seek(file_off)
                    chunk = f.read(n)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    written += len(chunk)
                    pos += len(chunk)
        type(self).bytes_served += written
        return written

    def do_HEAD(self):  # noqa: N802
        self.send_response(200)
        self.send_header("Content-Length", str(self.total))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "video/mp4")
        self.end_headers()

    def do_GET(self):  # noqa: N802
        rng = self.headers.get("Range")
        total = self.total
        if not rng:
            self.send_response(200)
            self.send_header("Content-Length", str(total))
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Type", "video/mp4")
            self.end_headers()
            try:
                self._stream(0, total - 1)
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        type(self).range_hits += 1
        spec = rng.split("=", 1)[1]
        start_s, _, end_s = spec.partition("-")
        start = int(start_s) if start_s else 0
        end = int(end_s) if end_s else total - 1
        end = min(end, total - 1)
        if start > end or start >= total:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{total}")
            self.end_headers()
            return
        self.send_response(206)
        self.send_header("Content-Range", f"bytes {start}-{end}/{total}")
        self.send_header("Content-Length", str(end - start + 1))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "video/mp4")
        self.end_headers()
        try:
            self._stream(start, end)
        except (BrokenPipeError, ConnectionResetError):
            pass


def start_server(v: dict, src: Path):
    class H(VirtualFaststartHandler):
        pass

    H.prefix = v["prefix"]
    H.moov = v["moov"]
    H.src_path = str(src)
    H.mdat_pos = v["mdat_pos"]
    H.mdat_size = v["mdat_size"]
    H.bytes_served = 0
    H.range_hits = 0
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{httpd.server_address[1]}/stream.mp4"
    return httpd, url, H


def spawn_hls(input_arg: str, mode: str, enc: str, ss_s: float, *, use_ss: bool) -> dict:
    out = Path(tempfile.mkdtemp(prefix="nj_mp4_"))
    start_number = int(ss_s * 1000 // 2000)
    args = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y"]
    if use_ss:
        args += ["-ss", f"{ss_s:.3f}"]
    args += ["-i", input_arg, "-output_ts_offset", f"{ss_s:.3f}", "-map", "0:v:0", "-map", "0:a:0?"]
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
        str(LEAD_SECS),
        str(out / "index.m3u8"),
    ]
    t0 = time.perf_counter()
    proc = subprocess.Popen(args, stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
    first_ms = None
    first_seg = None
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
        "under_3s": first_ms is not None and first_ms < GATE_MS,
        "err_tail": err,
        "out_dir": str(out),
        "rc": proc.returncode,
    }


def keyframe_near(path: Path, t: float) -> dict:
    """Nearest keyframe at or before t via ffprobe packets."""
    pr = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,pos,flags",
            "-of",
            "csv=p=0",
            "-read_intervals",
            f"{max(0, t - 15)}%{t + 1}",
            str(path),
        ],
        capture_output=True,
        text=True,
    )
    best = None
    for line in pr.stdout.splitlines():
        parts = line.split(",")
        if len(parts) < 3:
            continue
        try:
            pts = float(parts[0])
            pos = int(parts[1])
        except ValueError:
            continue
        flags = parts[2]
        if "K" not in flags:
            continue
        if pts <= t and (best is None or pts > best["pts_s"]):
            best = {"pts_s": pts, "pos": pos}
    return best or {}


def main():
    enc = pick_encoder()
    print(f"encoder={enc} src={SRC}", flush=True)
    if not SRC.is_file():
        raise SystemExit(f"missing {SRC}")

    t_build = time.perf_counter()
    v = build_virtual_faststart(SRC)
    build_ms = int((time.perf_counter() - t_build) * 1000)
    print(
        json.dumps(
            {
                "virtual_kind": v["kind"],
                "build_ms": build_ms,
                "delta": v["delta"],
                "offsets_rewritten": v["offsets_rewritten"],
                "prefix_len": len(v["prefix"]),
                "moov_len": len(v["moov"]),
                "mdat_size": v["mdat_size"],
            }
        ),
        flush=True,
    )
    if v["kind"] == "already_faststart":
        print("WARNING: source already faststart; pick an end_moov path", flush=True)

    kf = keyframe_near(SRC, SS_S)
    print(f"keyframe_near_{SS_S}={kf}", flush=True)

    rows = []

    # A: baseline -ss on real NAS path
    for mode in ("transcode", "copy"):
        print(f"run baseline_ss {mode}", flush=True)
        h = spawn_hls(str(SRC), mode, enc, SS_S, use_ss=True)
        rows.append({"shape": "baseline_ss_file", "mode": mode, **{k: h[k] for k in h if k != "out_dir"}})
        print(json.dumps(rows[-1], default=str), flush=True)
        shutil.rmtree(h["out_dir"], ignore_errors=True)

    # B: virtual faststart HTTP + -ss
    httpd, url, H = start_server(v, SRC)
    try:
        for mode in ("transcode", "copy"):
            print(f"run virtual_faststart_http_ss {mode}", flush=True)
            H.bytes_served = 0
            H.range_hits = 0
            h = spawn_hls(url, mode, enc, SS_S, use_ss=True)
            row = {
                "shape": "virtual_faststart_http_ss",
                "mode": mode,
                "http_bytes_served": H.bytes_served,
                "http_range_hits": H.range_hits,
                **{k: h[k] for k in h if k != "out_dir"},
            }
            rows.append(row)
            print(json.dumps(row, default=str), flush=True)
            shutil.rmtree(h["out_dir"], ignore_errors=True)
    finally:
        httpd.shutdown()

    # C: naive splice without stco rewrite (expect broken) — local temp small body
    if kf.get("pos"):
        print("run naive_splice_no_rewrite copy", flush=True)
        boxes = read_boxes(SRC)
        mdat = find_box(boxes, "mdat")
        moov = find_box(boxes, "moov")
        with open(SRC, "rb") as f:
            f.seek(0)
            head = f.read(mdat["pos"])  # ftyp(+free)
            f.seek(moov["pos"])
            moov_raw = f.read(moov["size"])
            f.seek(kf["pos"])
            body = f.read(32 * 1024 * 1024)
        # wrong: moov then partial mdat from keyframe without rewrite
        path = Path(tempfile.mkstemp(prefix="nj_naive_", suffix=".mp4")[1])
        path.write_bytes(head + moov_raw + body)
        h = spawn_hls(str(path), "copy", enc, SS_S, use_ss=False)
        rows.append(
            {
                "shape": "naive_splice_no_rewrite",
                "mode": "copy",
                "note": "expect fail or wrong sidx — Matroska splice must not be reused",
                **{k: h[k] for k in h if k != "out_dir"},
            }
        )
        print(json.dumps(rows[-1], default=str), flush=True)
        path.unlink(missing_ok=True)
        shutil.rmtree(h["out_dir"], ignore_errors=True)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    doc = {
        "src": str(SRC),
        "ss_s": SS_S,
        "encoder": enc,
        "virtual": {
            "kind": v["kind"],
            "build_ms": build_ms,
            "delta": v["delta"],
            "offsets_rewritten": v["offsets_rewritten"],
        },
        "keyframe": kf,
        "rows": rows,
        "mechanism_candidate": (
            "HTTP virtual faststart (moov before mdat via Range remap + stco/co64 "
            "rewrite) + -ss at map PTS. Not a Matroska Cluster splice."
        ),
    }
    OUT.write_text(json.dumps(doc, indent=2))
    print("wrote", OUT, flush=True)
    print("\n=== summary ===", flush=True)
    for r in rows:
        print(
            f"{r['shape']:32} {r['mode']:10} land={r.get('land_ms')} "
            f"sidx={r.get('sidx_ept_s')} under3={r.get('under_3s')}",
            flush=True,
        )


if __name__ == "__main__":
    main()
