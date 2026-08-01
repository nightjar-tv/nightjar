#!/usr/bin/env python3
"""A/B HDR→SDR colour compare (ADR-0022 tonemap vs old retag).

Runs the same filter strings Nightjar uses in
`server/crates/transcode/src/hls.rs` (HDR_TONEMAP_CHAIN / SDR_RETAG_CHAIN)
against local files and writes side-by-side stills + short clips.

This is the colour-judgment harness. It does not need a running Nightjar.
For a live session check, see --session at the bottom of --help.

Usage (from repo root):
  python3 scripts/hdr_tonemap_compare.py
  python3 scripts/hdr_tonemap_compare.py path/to/file.mp4
  python3 scripts/hdr_tonemap_compare.py --ss 2 --seconds 3

Defaults (if no paths given):
  testdata/files/hevc_hdr10_mp4.mp4
  testdata/.../Patterns_Of_Nature_HDR10-P8.1_FHD_24_....mp4

Output under OUT_DIR (default /tmp/nightjar-hdr-compare/<stamp>/):
  <stem>/retag.png      — old bug (labels only, no convert)
  <stem>/tonemap.png    — Nightjar graph (zscale + hable)
  <stem>/retag.mp4      —  short clip, same filters
  <stem>/tonemap.mp4
  compare.html          — open in a browser for side-by-side

Requires FFmpeg with libzimg (`zscale` in `ffmpeg -filters`).
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

# Keep in sync with server/crates/transcode/src/hls.rs
SDR_RETAG = (
    "sidedata=delete,"
    "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709"
)
HDR_TONEMAP = (
    "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,"
    "tonemap=tonemap=hable:desat=0,"
    "zscale=t=bt709:m=bt709:r=tv,format=yuv420p,sidedata=delete"
)

REPO = Path(__file__).resolve().parents[1]
DEFAULT_SOURCES = [
    REPO / "testdata/files/hevc_hdr10_mp4.mp4",
    REPO
    / "testdata/files/dolby-vision-browser-kit/24fps/FHD"
    / "Patterns_Of_Nature_HDR10-P8.1_FHD_24_H265-4Mbps_DD+JOC-768Kbps.mp4",
]


def run(cmd: list[str]) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True)


def ffmpeg_has_zscale() -> bool:
    out = subprocess.run(
        ["ffmpeg", "-hide_banner", "-filters"],
        capture_output=True,
        text=True,
        check=False,
    )
    text = out.stdout or out.stderr
    for line in text.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1] == "zscale":
            return True
    return False


def probe_hdr(path: Path) -> str:
    out = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_transfer,color_primaries,color_space",
            "-of",
            "default=nw=1",
            str(path),
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    return out.stdout.strip().replace("\n", " ")


def encode_variant(
    src: Path,
    out_dir: Path,
    name: str,
    vf: str,
    ss: float,
    seconds: float,
) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    png = out_dir / f"{name}.png"
    mp4 = out_dir / f"{name}.mp4"
    # Still
    run(
        [
            "ffmpeg",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            str(ss),
            "-t",
            "0.2",
            "-i",
            str(src),
            "-vf",
            vf,
            "-frames:v",
            "1",
            "-update",
            "1",
            str(png),
        ]
    )
    # Short clip (silent) for scrubbing in QuickTime / browser
    run(
        [
            "ffmpeg",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            str(ss),
            "-t",
            str(seconds),
            "-i",
            str(src),
            "-vf",
            vf,
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            str(mp4),
        ]
    )


def write_compare_html(out_root: Path, stems: list[str]) -> Path:
    rows = []
    for stem in stems:
        rows.append(
            f"""
<section>
  <h2>{stem}</h2>
  <div class="row">
    <figure>
      <img src="{stem}/retag.png" alt="retag" />
      <figcaption>retag only (broken / green-purple class)</figcaption>
    </figure>
    <figure>
      <img src="{stem}/tonemap.png" alt="tonemap" />
      <figcaption>Nightjar tonemap (zscale + hable)</figcaption>
    </figure>
  </div>
  <p>
    Clips:
    <a href="{stem}/retag.mp4">retag.mp4</a> ·
    <a href="{stem}/tonemap.mp4">tonemap.mp4</a>
  </p>
</section>
"""
        )
    html = f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Nightjar HDR tonemap compare</title>
  <style>
    body {{ font-family: system-ui, sans-serif; margin: 1.5rem; background: #111; color: #eee; }}
    .row {{ display: flex; gap: 1rem; flex-wrap: wrap; }}
    figure {{ margin: 0; max-width: 48%; }}
    img {{ width: 100%; height: auto; background: #000; }}
    figcaption {{ margin-top: 0.4rem; font-size: 0.9rem; color: #bbb; }}
    a {{ color: #8cf; }}
  </style>
</head>
<body>
  <h1>HDR → SDR compare</h1>
  <p>Left = old retag. Right = Nightjar tonemap. Open the mp4s if the still is ambiguous.</p>
  {"".join(rows)}
</body>
</html>
"""
    path = out_root / "compare.html"
    path.write_text(html)
    return path


def http_json(url: str, method: str = "GET", timeout: float = 60.0):
    req = urllib.request.Request(url, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        body = resp.read()
        if not body:
            return None
        return __import__("json").loads(body)


def session_frame(base: str, item_id: int, out: Path, ss_ms: int) -> None:
    """Start a BROWSER_V0 session and grab one decoded frame from HLS output."""
    base = base.rstrip("/")
    info = http_json(f"{base}/api/v0/items/{item_id}/playback-info?profileId=BROWSER_V0")
    print("playback-info:", info.get("playbackMethod"), info.get("reason"))
    if info.get("playbackMethod") != "transcode":
        raise SystemExit(
            f"expected transcode for browser HDR (got {info.get('playbackMethod')})"
        )
    sess = http_json(
        f"{base}/api/v0/items/{item_id}/sessions?startMs={ss_ms}&profileId=BROWSER_V0",
        method="POST",
    )
    session_id = sess["sessionId"]
    playlist = sess["playlistUrl"]
    if playlist.startswith("/"):
        playlist = base + playlist
    print("session:", session_id, "playlist:", playlist)
    # Wait briefly for first media, then decode one frame via ffmpeg
    time.sleep(2.5)
    out.parent.mkdir(parents=True, exist_ok=True)
    run(
        [
            "ffmpeg",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            playlist,
            "-frames:v",
            "1",
            "-update",
            "1",
            str(out),
        ]
    )
    print("wrote", out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "paths",
        nargs="*",
        type=Path,
        help="Source media (default: HDR10 corpus + P8.1 FHD kit)",
    )
    ap.add_argument("--ss", type=float, default=2.0, help="Seek seconds (default 2)")
    ap.add_argument(
        "--seconds", type=float, default=3.0, help="Clip length seconds (default 3)"
    )
    ap.add_argument(
        "--out",
        type=Path,
        default=None,
        help="Output root (default /tmp/nightjar-hdr-compare/<stamp>)",
    )
    ap.add_argument(
        "--session",
        type=int,
        metavar="ITEM_ID",
        help="Also pull one frame from a live Nightjar session (needs --base)",
    )
    ap.add_argument(
        "--base",
        default=os.environ.get("BASE", "http://127.0.0.1:8096"),
        help="Nightjar base URL for --session (or env BASE)",
    )
    args = ap.parse_args()

    if not ffmpeg_has_zscale():
        print(
            "error: FFmpeg has no zscale (need --enable-libzimg). "
            "Refusing to run a fake tonemap.",
            file=sys.stderr,
        )
        return 1

    sources = [p for p in (args.paths or DEFAULT_SOURCES) if p.is_file()]
    missing = [p for p in (args.paths or DEFAULT_SOURCES) if not p.is_file()]
    for p in missing:
        print(f"skip missing: {p}", file=sys.stderr)
    if not sources and args.session is None:
        print("error: no source files found", file=sys.stderr)
        return 1

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_root = args.out or Path(f"/tmp/nightjar-hdr-compare/{stamp}")
    out_root.mkdir(parents=True, exist_ok=True)

    stems: list[str] = []
    for src in sources:
        print(f"\n=== {src} ===")
        print("probe:", probe_hdr(src))
        stem = src.stem[:80]
        stems.append(stem)
        dest = out_root / stem
        encode_variant(src, dest, "retag", SDR_RETAG, args.ss, args.seconds)
        encode_variant(src, dest, "tonemap", HDR_TONEMAP, args.ss, args.seconds)

    html = write_compare_html(out_root, stems) if stems else None

    if args.session is not None:
        session_out = out_root / f"session_item{args.session}.png"
        try:
            session_frame(args.base, args.session, session_out, int(args.ss * 1000))
        except urllib.error.URLError as e:
            print(f"session failed ({e}); local A/B still written", file=sys.stderr)
            return 1

    print("\n=== results ===")
    print(out_root)
    if html:
        print(f"open {html}")
        print(f"open {out_root}")  # Finder / file manager
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
