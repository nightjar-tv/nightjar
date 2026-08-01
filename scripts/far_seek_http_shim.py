#!/usr/bin/env python3
"""Production I/O shapes for Cluster-offset spawn: temp file vs HTTP range shim.

Nightjar would serve header+remainder via HTTP Range; ffmpeg opens the URL
(seekable). Compare wall/land/PTS to the temp-synth path already measured.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from far_seek_cluster_spawn import (  # noqa: E402
    UP,
    CASES,
    GATE_MS,
    build_synthetic,
    find_cluster_before,
    find_first_cluster,
    parse_sidx_ept,
    pick_encoder,
    pts_ok,
    spawn_hls,
)

BODY = 16 * 1024 * 1024  # first-segment sized; full-remainder is a later cost model
OUT = Path(
    "/Users/gmacarthur/Documents/GitHub/nightjar-meta/notes/far-seek-http-shim-2026-08-01.json"
)


CHUNK = 256 * 1024


class NasBackedHandler(BaseHTTPRequestHandler):
    """Seekable virtual mkv: local header + NAS body from cluster_pos to EOF.

    Range requests past the header are satisfied by pread on the source file.
    Responses stream in CHUNK-sized pieces — never buffer Cluster→EOF into RAM
    (a far Bluray remainder is multi-GB; that hung the first probe at ~15GB).
    """

    header: bytes = b""
    src_path: str = ""
    cluster_pos: int = 0
    src_size: int = 0
    bytes_served: int = 0
    range_hits: int = 0

    @property
    def body_size(self) -> int:
        return max(0, self.src_size - self.cluster_pos)

    @property
    def total(self) -> int:
        return len(self.header) + self.body_size

    def log_message(self, fmt, *args):  # noqa: A003
        pass

    def _stream(self, start: int, end: int) -> int:
        """Write bytes [start, end] inclusive; return bytes written."""
        h = len(self.header)
        written = 0
        pos = start
        with open(self.src_path, "rb") as f:
            while pos <= end:
                n = min(CHUNK, end - pos + 1)
                if pos < h:
                    take = min(n, h - pos)
                    self.wfile.write(self.header[pos : pos + take])
                    written += take
                    pos += take
                else:
                    file_off = self.cluster_pos + (pos - h)
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
        self.send_header("Content-Type", "video/x-matroska")
        self.end_headers()

    def do_GET(self):  # noqa: N802
        rng = self.headers.get("Range")
        total = self.total
        if not rng:
            # Full GET of a multi-GB virtual file must stream; never assemble in RAM.
            self.send_response(200)
            self.send_header("Content-Length", str(total))
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Type", "video/x-matroska")
            self.end_headers()
            try:
                self._stream(0, total - 1)
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        assert rng.startswith("bytes=")
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
        length = end - start + 1
        self.send_response(206)
        self.send_header("Content-Range", f"bytes {start}-{end}/{total}")
        self.send_header("Content-Length", str(length))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Type", "video/x-matroska")
        self.end_headers()
        try:
            self._stream(start, end)
        except (BrokenPipeError, ConnectionResetError):
            pass


def start_nas_shim(header: bytes, src: Path, cluster_pos: int):
    class H(NasBackedHandler):
        pass

    H.header = header
    H.src_path = str(src)
    H.cluster_pos = cluster_pos
    H.src_size = src.stat().st_size
    H.bytes_served = 0
    H.range_hits = 0
    # Threading: ffmpeg's HTTP client opens concurrent Range connections;
    # a single-threaded server deadlocks (0 bytes served, 90s timeout).
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
    port = httpd.server_address[1]
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, f"http://127.0.0.1:{port}/stream.mkv", H


def spawn_hls_url(url: str, mode: str, encoder: str, offset_s: float) -> dict:
    """Like spawn_hls but -i URL (ffmpeg HTTP)."""
    out = Path(tempfile.mkdtemp(prefix="nj_http_"))
    start_number = int(offset_s * 1000 // 2000)
    force_kf = "expr:gte(t,n_forced*2.0)"
    args = [
        "ffmpeg",
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-i",
        url,
        "-output_ts_offset",
        f"{offset_s:.3f}",
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
    deadline = t0 + 90
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
        err = proc.stderr.read().decode("utf-8", "replace")[-1500:]
    sidx = parse_sidx_ept(first_seg) if first_seg else None
    return {
        "land_ms": first_ms,
        "first_seg": first_seg.name if first_seg else None,
        "sidx_ept_s": sidx,
        "out_dir": str(out),
        "err_tail": err,
        "rc": proc.returncode,
    }


def load_parts(src: Path, header_end: int, cluster_pos: int, body_n: int) -> tuple[bytes, bytes, dict]:
    t0 = time.perf_counter()
    with open(src, "rb") as f:
        header = f.read(header_end)
        f.seek(cluster_pos)
        body = f.read(body_n)
    return header, body, {"read_ms": int((time.perf_counter() - t0) * 1000), "body": len(body)}


def main():
    enc = pick_encoder()
    print(f"encoder={enc}", flush=True)
    header_end = find_first_cluster(UP)
    print(f"header_end={header_end}", flush=True)
    rows = []

    for case in CASES:
        cluster = find_cluster_before(UP, case["pkt_pos"])
        print(f"case={case['label']} cluster={cluster}", flush=True)
        synth, _ = build_synthetic(UP, header_end, cluster, BODY)
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
                str(synth),
            ],
            capture_output=True,
            text=True,
        )
        cluster_pts = float(pr.stdout.strip().splitlines()[0].split(",")[0])
        synth.unlink()
        print(f"  cluster_pts={cluster_pts}", flush=True)

        for mode in ("transcode", "copy"):
            offset = case["ss_s"] if mode == "transcode" else cluster_pts
            advertise_target = case["ss_s"]

            for shape in ("temp_16mb", "http_nas_remainder"):
                for rep in range(2):
                    print(f"  run {case['label']} {mode} {shape} rep{rep}", flush=True)
                    wall0 = time.perf_counter()
                    served = None
                    ranges = None
                    virtual_total = None
                    if shape == "temp_16mb":
                        header, body, io = load_parts(UP, header_end, cluster, BODY)
                        path = Path(tempfile.mkstemp(prefix="nj_t_", suffix=".mkv")[1])
                        path.write_bytes(header + body)
                        hls = spawn_hls(path, mode, enc, offset, use_ts_offset=True)
                        path.unlink(missing_ok=True)
                        read_ms = io["read_ms"]
                        land = hls.get("encode_ms")
                    else:
                        t_hdr = time.perf_counter()
                        with open(UP, "rb") as f:
                            header = f.read(header_end)
                        read_ms = int((time.perf_counter() - t_hdr) * 1000)
                        httpd, url, H = start_nas_shim(header, UP, cluster)
                        virtual_total = len(header) + max(0, H.src_size - cluster)
                        try:
                            hls = spawn_hls_url(url, mode, enc, offset)
                        finally:
                            httpd.shutdown()
                        land = hls.get("land_ms")
                        served = H.bytes_served
                        ranges = H.range_hits
                    wall_ms = int((time.perf_counter() - wall0) * 1000)
                    sidx = hls.get("sidx_ept_s")
                    row = {
                        "label": case["label"],
                        "mode": mode,
                        "shape": shape,
                        "rep": rep,
                        "offset_s": offset,
                        "requested_s": case["ss_s"],
                        "cluster_pts_s": cluster_pts,
                        "header_read_ms": read_ms,
                        "wall_ms": wall_ms,
                        "land_ms": land,
                        "under_3s": bool(land is not None and land < GATE_MS),
                        "sidx_ept_s": sidx,
                        "delta_vs_request": None
                        if sidx is None
                        else sidx - advertise_target,
                        "delta_vs_cluster": None if sidx is None else sidx - cluster_pts,
                        "honest_vs_request": sidx is not None
                        and abs(sidx - advertise_target) <= 0.5,
                        "honest_vs_cluster": sidx is not None
                        and abs(sidx - cluster_pts) <= 0.5,
                        "http_bytes_served": served,
                        "http_range_hits": ranges,
                        "virtual_total_bytes": virtual_total,
                        "first_seg": hls.get("first_seg"),
                        "err_tail": (hls.get("err_tail") or "")[:400],
                    }
                    rows.append(row)
                    print(
                        json.dumps({k: row[k] for k in row if k != "err_tail"}),
                        flush=True,
                    )
                    shutil.rmtree(hls["out_dir"], ignore_errors=True)
                    time.sleep(0.3)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(
        json.dumps(
            {
                "encoder": enc,
                "note": (
                    "temp_16mb: prior probe shape (copy header+16MiB to local file). "
                    "http_nas_remainder: production candidate — Content-Length is "
                    "header+Cluster→EOF; body streams via Range (no temp, seekable). "
                    "under_3s uses land_ms (first seg), not wall (includes setup)."
                ),
                "rows": rows,
            },
            indent=2,
        )
    )
    print("wrote", OUT, flush=True)
    print("\n=== rep0 ===", flush=True)
    for r in rows:
        if r["rep"] != 0:
            continue
        print(
            f"{r['label']:4} {r['mode']:10} {r['shape']:20} "
            f"wall={r['wall_ms']:5} land={r['land_ms']} "
            f"sidx={r['sidx_ept_s']} Δreq={r['delta_vs_request']} "
            f"Δclu={r['delta_vs_cluster']} served={r['http_bytes_served']} "
            f"ranges={r['http_range_hits']}",
            flush=True,
        )


if __name__ == "__main__":
    main()
