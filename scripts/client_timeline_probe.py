#!/usr/bin/env python3
"""Measure what a browser client waits for, per title, per playback path.

Sits next to `keyframe_index_probe.py` and imports its container parsers, so
keep both files in the same directory.

For each title it derives the BROWSER_V0 playback method from stored probe
columns, then measures only the reads that method actually needs:

  direct play (MP4-family)   header/moov fetch -> first media bytes -> cold
                             seek at --seek-ms. No ffmpeg. Embedded text needs
                             a standalone extract, because nothing else reads
                             the file.
  remux / transcode          keyframe index read -> cold land read at
                             --seek-ms (what the ADR-0023 virtual file does on
                             its first binds). Subtitles come out of the
                             session for free.

Read-only. Writes one CSV row per title and nothing else.

    python3 client_timeline_probe.py \
        --db ~/nightjar.db.copy --map "/media=/Volumes/media" \
        --sample 120 --out client-timeline-2026-08-06.csv

Add --subs to also time a real standalone subtitle extract (full source read;
use a small sample). Add --purge-hint to be reminded to drop caches between
runs — cold numbers are the only ones that mean anything here.
"""

import argparse
import csv
import hashlib
import io
import os
import random
import signal
import sqlite3
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    from keyframe_index_probe import (CountingReader, detect, mkv_parse,
                                      mp4_parse)
except ImportError:
    sys.exit("keyframe_index_probe.py must sit in the same directory")

# BROWSER_V0, mirrored from server/crates/core/src/playback.rs. This is an
# approximation for measurement only: the server owns the real decision, and
# any disagreement between this and decide_playback is a finding about this
# script, not about the server.
# NOTE: "webm" is deliberately absent from the format_name set. ffprobe reports
# Matroska as "matroska,webm", so accepting webm here would classify every MKV
# as direct play — the opposite of what decide_playback does (its own test
# asserts MKV + h264 + aac -> Remux). WebM is accepted by extension instead.
BROWSER_CONTAINERS = {"mp4", "mov", "m4a", "3gp", "3g2", "mj2"}
BROWSER_EXTENSIONS = {".mp4", ".m4v", ".mov", ".webm"}
BROWSER_VIDEO = {"h264", "vp8", "vp9", "av1"}
BROWSER_AUDIO = {"aac", "mp3", "opus", "vorbis", "flac"}
MAX_CHANNELS = 2

FIELDS = [
    "path", "method", "method_reason", "container", "video_codec", "audio_codec",
    "audio_channels", "height", "hdr", "duration_ms", "size_gb",
    "subtitle_class", "n_text_tracks",
    "header_ms", "header_bytes", "moov_position",
    "index_source", "n_entries", "coverage_pct",
    "first_bytes_ms", "seek_land_ms", "seek_land_offset",
    "t_playable_ms", "t_seek_ms",
    "subs_ms", "subs_bytes_out", "subs_needed_standalone",
    "ok", "error",
]

FIRST_BYTES = 2 * 1024 * 1024      # what a player pulls before it can paint
LAND_BYTES = 256 * 1024            # first read at a seek land
_stop = False


def hash_path(path, lib_id):
    """Stable, non-reversible stand-in for a real path: keeps the library
    split (several summaries break down by it) without naming a title."""
    digest = hashlib.sha256(path.encode("utf-8")).hexdigest()[:8]
    return f"lib{lib_id}:{digest}"


def _on_sigint(signum, frame):
    global _stop
    _stop = True
    sys.stderr.write("\ninterrupt; finishing this title then stopping\n")


def decide(path, container, video, audio, channels, height, hdr, probe_status):
    """(method, reason). Session means remux or transcode; the distinction is
    whether codecs are acceptable."""
    if probe_status != "probed":
        return "transcode", f"probe {probe_status}"
    parts = {p.strip() for p in (container or "").lower().split(",") if p.strip()}
    ext = os.path.splitext(path)[1].lower()
    container_ok = ext in BROWSER_EXTENSIONS or bool(parts & BROWSER_CONTAINERS)
    video_ok = (video or "").lower() in BROWSER_VIDEO
    audio_ok = (audio or "").lower() in BROWSER_AUDIO

    if not video_ok:
        return "transcode", f"video codec {video}"
    if not audio_ok:
        return "transcode", f"audio codec {audio}"
    if hdr and hdr.lower() not in ("", "none", "sdr"):
        return "transcode", f"hdr {hdr}"
    if not container_ok:
        return "remux", f"container {container}"
    if channels is None:
        return "remux", "channel count not stored"
    if channels > MAX_CHANNELS:
        return "remux", f"{channels} channels over browser ceiling"
    return "direct_play", "codecs and container acceptable"


def timed_read(path, offset, length):
    """Cold-ish sequential read at an offset; returns (ms, bytes)."""
    t0 = time.monotonic()
    with open(path, "rb", buffering=0) as fh:
        fh.seek(offset)
        got = fh.read(length)
    return int((time.monotonic() - t0) * 1000), len(got)


def read_index(path, kind):
    t0 = time.monotonic()
    r = CountingReader(path)
    try:
        got = mkv_parse(r) if kind == "matroska" else mp4_parse(r)
    finally:
        b, n = r.bytes_read, r.n_reads
        r.close()
    return got, int((time.monotonic() - t0) * 1000), b, n


def text_track_indices(path, timeout):
    """Relative subtitle-stream indices whose codec is text or ASS. Image
    formats are excluded: they cannot become WebVTT, and mapping one into the
    command would fail the whole invocation."""
    cmd = ["ffprobe", "-v", "error", "-select_streams", "s",
           "-show_entries", "stream=codec_name", "-of", "csv=p=0", path]
    try:
        proc = subprocess.run(cmd, capture_output=True, timeout=timeout, text=True)
    except subprocess.TimeoutExpired:
        return None
    if proc.returncode != 0:
        return None
    image = {"hdmv_pgs_subtitle", "dvd_subtitle", "dvb_subtitle", "xsub",
             "pgssub", "dvdsub", "dvbsub", "dvb_teletext"}
    out = []
    for rel, line in enumerate(
            [l.strip().lower() for l in proc.stdout.splitlines() if l.strip()]):
        if line not in image:
            out.append(rel)
    return out


def extract_subs(path, rel_indices, out_dir, timeout):
    """One demux, every text track out as WebVTT — the ADR-0013 §3 shape.
    Returns (ms, total bytes written) or (ms, -1) on failure."""
    os.makedirs(out_dir, exist_ok=True)
    cmd = ["ffmpeg", "-v", "error", "-y", "-i", path]
    written = []
    for rel in rel_indices:
        dest = os.path.join(out_dir, f"t{rel}.vtt")
        written.append(dest)
        cmd += ["-map", f"0:s:{rel}", "-c:s", "webvtt", dest]
    t0 = time.monotonic()
    try:
        proc = subprocess.run(cmd, capture_output=True, timeout=timeout, text=True)
    except subprocess.TimeoutExpired:
        return int((time.monotonic() - t0) * 1000), -1
    ms = int((time.monotonic() - t0) * 1000)
    total = 0
    for dest in written:
        try:
            total += os.path.getsize(dest)
            os.remove(dest)
        except OSError:
            pass
    return ms, (total if proc.returncode == 0 else -1)


def probe_title(item, args, scratch):
    path = item["path"]
    out_path = hash_path(path, item["lib"]) if args.hash_paths else path

    def done(row):
        if args.hash_paths:
            # ffprobe/ffmpeg/OS error text can embed the real path (e.g.
            # "No such file: '/Volumes/...'"); scrub any leftover copy of
            # it out of every column, not just `path`.
            for k, v in row.items():
                if isinstance(v, str) and path in v:
                    row[k] = v.replace(path, out_path)
        row["path"] = out_path
        return row

    row = {f: "" for f in FIELDS}
    row.update(
        path=path, container=item["container"], video_codec=item["video_codec"],
        audio_codec=item["audio_codec"], audio_channels=item["audio_channels"],
        height=item["height"], hdr=item["hdr"], duration_ms=item["duration_ms"],
    )
    method, reason = decide(
        path, item["container"], item["video_codec"], item["audio_codec"],
        item["audio_channels"], item["height"], item["hdr"], item["probe_status"])
    row.update(method=method, method_reason=reason)

    try:
        row["size_gb"] = round(os.path.getsize(path) / 1073741824.0, 2)
    except OSError as exc:
        row.update(ok="0", error=f"stat: {exc}")
        return done(row)

    kind = detect(path)
    try:
        got, index_ms, ibytes, _ = read_index(path, kind)
    except Exception as exc:
        row.update(ok="0", error=f"index: {type(exc).__name__}: {exc}"[:180])
        return done(row)

    entries = got.get("entries") or []
    dur = got.get("duration_ms") or item["duration_ms"]
    last = entries[-1][0] if entries else None
    row.update(
        header_ms=index_ms, header_bytes=ibytes,
        index_source=got.get("index_source", ""), n_entries=len(entries),
        moov_position=got.get("moov_position", ""),
        coverage_pct=(round(100.0 * last / dur, 1) if (last and dur) else ""),
    )

    text_rel = text_track_indices(path, args.probe_timeout)
    n_text = -1 if text_rel is None else len(text_rel)
    row["n_text_tracks"] = n_text
    row["subs_needed_standalone"] = "1" if (method == "direct_play" and n_text > 0) else "0"

    seek_ms = args.seek_ms
    if method == "direct_play":
        # A player needs the header, then media bytes, before it paints.
        first_ms, _ = timed_read(path, header_end_guess(entries), FIRST_BYTES)
        row["first_bytes_ms"] = first_ms
        row["t_playable_ms"] = index_ms + first_ms
        land = offset_at(entries, seek_ms)
        if land is not None:
            land_ms, _ = timed_read(path, land, LAND_BYTES)
            row.update(seek_land_offset=land, seek_land_ms=land_ms,
                       t_seek_ms=land_ms)
        else:
            row["t_seek_ms"] = ""       # no index: browser must hunt
    else:
        # Session start: the virtual file reads the header, then the land.
        land = offset_at(entries, seek_ms)
        if land is not None:
            land_ms, _ = timed_read(path, land, LAND_BYTES)
            row.update(seek_land_offset=land, seek_land_ms=land_ms,
                       t_seek_ms=index_ms + land_ms)
        row["t_playable_ms"] = index_ms

    if args.subs and text_rel:
        sms, sbytes = extract_subs(path, text_rel, scratch, args.subs_timeout)
        row.update(subs_ms=sms, subs_bytes_out=sbytes)

    row["ok"] = "1"
    return done(row)


def header_end_guess(entries):
    return entries[0][1] if entries else 0


def offset_at(entries, pts_ms):
    best = None
    for pts, off in entries:
        if pts <= pts_ms:
            best = off
        else:
            break
    return best


def items_from_db(db, mappings):
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    rows = con.execute("""
        SELECT l.id AS lib, l.path AS lib_path, m.path AS rel, m.container,
               m.video_codec, m.audio_codec, m.audio_channels, m.height,
               m.hdr, m.duration_ms, m.probe_status
        FROM media_items m JOIN libraries l ON l.id = m.library_id
        WHERE m.probe_status = 'probed'
        ORDER BY l.id, m.path""").fetchall()
    con.close()
    out = []
    for r in rows:
        p = os.path.join(r["lib_path"], r["rel"])
        for frm, to in mappings:
            if p == frm or p.startswith(frm + "/"):
                p = to + p[len(frm):]
                break
        d = dict(r)
        d["path"] = p
        out.append(d)
    return out


def stratified(items, n, rng):
    if n <= 0 or n >= len(items):
        return items
    groups = {}
    for it in items:
        groups.setdefault(it["lib"], []).append(it)
    picked = []
    for lib, g in sorted(groups.items()):
        take = max(1, round(n * len(g) / len(items)))
        picked.extend(rng.sample(g, min(take, len(g))))
    rng.shuffle(picked)
    return picked[:n]


def stat_line(label, vals):
    if not vals:
        return
    vals = sorted(vals)
    mid = vals[len(vals) // 2]
    p95 = vals[min(int(len(vals) * 0.95), len(vals) - 1)]
    print(f"    {label:<16} median {mid:>8,}  p95 {p95:>8,}  max {vals[-1]:>8,}")


def summarise(out):
    rows = [r for r in csv.DictReader(open(out, newline=""))]
    ok = [r for r in rows if r["ok"] == "1"]
    print(f"\n--- summary ---\n{len(rows)} titles, {len(ok)} ok, "
          f"{len(rows) - len(ok)} failed")

    print("\nmethod mix (BROWSER_V0):")
    for m in ("direct_play", "remux", "transcode"):
        sub = [r for r in ok if r["method"] == m]
        if not sub:
            continue
        print(f"  {m:<12} {len(sub):>5}  {100.0 * len(sub) / len(ok):5.1f}%")
        reasons = {}
        for r in sub:
            reasons[r["method_reason"]] = reasons.get(r["method_reason"], 0) + 1
        for k, v in sorted(reasons.items(), key=lambda x: -x[1])[:4]:
            print(f"      {v:>5}  {k}")
        stat_line("time to playable", [int(r["t_playable_ms"]) for r in sub
                                       if r["t_playable_ms"] != ""])
        stat_line("time to seek", [int(r["t_seek_ms"]) for r in sub
                                   if r["t_seek_ms"] != ""])
        stat_line("index read ms", [int(r["header_ms"]) for r in sub
                                    if r["header_ms"] != ""])
        stat_line("index bytes", [int(r["header_bytes"]) for r in sub
                                  if r["header_bytes"] != ""])
        no_index = [r for r in sub if int(r["n_entries"] or 0) == 0]
        if no_index:
            print(f"      no usable index: {len(no_index)} of {len(sub)}")

    need = [r for r in ok if r["subs_needed_standalone"] == "1"]
    print(f"\nstandalone subtitle extract required: {len(need)} of {len(ok)} "
          f"({100.0 * len(need) / max(len(ok), 1):.1f}%)")
    print("  (direct play with embedded text; every other path gets subtitles "
          "from the session that is already reading)")
    subs = [r for r in ok if r["subs_ms"] not in ("", None) and int(r["subs_ms"]) > 0]
    if subs:
        stat_line("extract ms", [int(r["subs_ms"]) for r in subs])
        stat_line("vtt bytes out", [int(r["subs_bytes_out"]) for r in subs
                                    if r["subs_bytes_out"] != ""])

    mp4 = [r for r in ok if r["moov_position"] not in ("", None)]
    if mp4:
        pos = {}
        for r in mp4:
            pos[r["moov_position"]] = pos.get(r["moov_position"], 0) + 1
        print("\nMP4 moov position:", ", ".join(f"{k}={v}" for k, v in sorted(pos.items())))
    short = [r for r in ok if r["coverage_pct"] not in ("", None)
             and float(r["coverage_pct"]) < 98.0]
    print(f"index coverage under 98% (truncated / damaged): {len(short)} of {len(ok)}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", required=True)
    ap.add_argument("--map", action="append", metavar="FROM=TO", default=[])
    ap.add_argument("--out", required=True)
    ap.add_argument("--sample", type=int, default=0)
    ap.add_argument("--seed", type=int, default=20260806)
    ap.add_argument("--seek-ms", type=int, default=1_200_000,
                    help="seek position to simulate, default 20 min")
    ap.add_argument("--subs", action="store_true",
                    help="also time a real standalone extract (full source read)")
    ap.add_argument("--subs-timeout", type=int, default=1800)
    ap.add_argument("--probe-timeout", type=int, default=30)
    ap.add_argument("--scratch", default="/tmp/nightjar-subs-probe")
    ap.add_argument("--only", choices=("all", "direct_play", "session"),
                    default="all",
                    help="restrict to titles whose derived method matches. "
                         "direct_play is the standalone-extract population: "
                         "combine with --subs to time the case nothing else "
                         "reads the file for.")
    ap.add_argument("--hash-paths", action="store_true",
                    help="write a stable lib{N}:{hash8} stand-in for `path` "
                         "instead of the real path; the file itself is still "
                         "read under the real path, only the CSV changes")
    ap.add_argument("--summary-only", action="store_true")
    args = ap.parse_args()

    if args.summary_only:
        summarise(args.out)
        return

    mappings = sorted(
        ((s.split("=", 1)[0].rstrip("/"), s.split("=", 1)[1].rstrip("/"))
         for s in args.map), key=lambda p: -len(p[0]))
    items = items_from_db(args.db, mappings)
    print(f"{len(items)} probed titles in source")

    if args.only != "all":
        before = len(items)
        keep = []
        for it in items:
            m, _ = decide(it["path"], it["container"], it["video_codec"],
                          it["audio_codec"], it["audio_channels"], it["height"],
                          it["hdr"], it["probe_status"])
            if (args.only == "direct_play" and m == "direct_play") or \
               (args.only == "session" and m in ("remux", "transcode")):
                keep.append(it)
        items = keep
        print(f"--only {args.only}: {len(items)} of {before} "
              f"({100.0 * len(items) / max(before, 1):.1f}%) by derived method")
        if not items:
            print("nothing matches; check --map resolves and the DB has probe columns")
            return

    if args.sample:
        items = stratified(items, args.sample, random.Random(args.seed))
        print(f"sampled down to {len(items)}")

    done = set()
    if os.path.exists(args.out):
        done = {r["path"] for r in csv.DictReader(open(args.out, newline=""))}
        print(f"{len(done)} already done; skipping")

    def key(it):
        return hash_path(it["path"], it["lib"]) if args.hash_paths else it["path"]

    todo = [it for it in items if key(it) not in done]
    if not todo:
        summarise(args.out)
        return

    print(f"\nseek simulated at {args.seek_ms / 60000:.1f} min. "
          f"Cold numbers only mean something on a cold cache — drop caches "
          f"between runs if you care about the absolute values.\n")

    signal.signal(signal.SIGINT, _on_sigint)
    new = not os.path.exists(args.out)
    t0 = time.monotonic()
    with open(args.out, "a", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=FIELDS)
        if new:
            w.writeheader()
        for i, item in enumerate(todo, start=1):
            if _stop:
                break
            w.writerow(probe_title(item, args, args.scratch))
            fh.flush()
            if i % 10 == 0 or i == len(todo):
                rate = i / max(time.monotonic() - t0, 1e-3)
                print(f"  {i}/{len(todo)}  {rate:.2f}/s  "
                      f"~{(len(todo) - i) / rate / 60:.0f} min left", flush=True)
    summarise(args.out)


if __name__ == "__main__":
    main()
