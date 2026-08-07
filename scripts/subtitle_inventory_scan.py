#!/usr/bin/env python3
"""Measure the subtitle stream inventory of a media library.

Reads container headers only (ffprobe), one file at a time, and writes one CSV
row per file. Answers: how many items actually carry embedded text subtitle
tracks, and how many carry nothing extractable.

Nothing is written to the database. Nothing is written to the media share.

Typical run, paths taken from a *copy* of the dogfood DB:

    python3 subtitle_inventory_scan.py \
        --db ~/nightjar.db.copy \
        --map /media=/Volumes/media \
        --sample 500 \
        --out subtitle-inventory-2026-08-06.csv

Then the full set by dropping --sample. Re-running appends: paths already in the
CSV are skipped, so an interrupted run resumes for free.

Or walk the share directly, no DB:

    python3 subtitle_inventory_scan.py \
        --walk "/Volumes/media/Movies" --walk "/Volumes/media/TV Shows" \
        --out subtitle-inventory-2026-08-06.csv
"""

import argparse
import csv
import hashlib
import json
import os
import random
import signal
import sqlite3
import subprocess
import sys
import time

# ffprobe codec_name values. Kept explicit rather than pattern-matched: an
# unrecognised codec must fall to "unknown" and be counted, not silently
# classified as harmless.
TEXT_CODECS = {
    "subrip", "srt", "mov_text", "webvtt", "text", "subviewer",
    "subviewer1", "microdvd", "mpl2", "jacosub", "sami", "realtext",
    "stl", "pjs", "vplayer", "eia_608", "subrip_text",
}
ASS_CODECS = {"ass", "ssa"}
IMAGE_CODECS = {
    "hdmv_pgs_subtitle", "pgssub", "dvd_subtitle", "dvdsub",
    "dvb_subtitle", "dvbsub", "xsub", "dvb_teletext",
}

MEDIA_EXTS = {".mkv", ".mp4", ".m4v", ".avi", ".mov", ".ts", ".m2ts", ".wmv", ".webm"}
SIDECAR_EXTS = {".srt", ".vtt", ".ass", ".ssa", ".sub"}

FIELDS = [
    "path", "ok", "container", "n_text", "n_ass", "n_image", "n_unknown",
    "sidecars", "classification", "size_gb", "probe_ms", "streams", "error",
]

_stop = False


def hash_path(path, lib_id):
    """Stable, non-reversible stand-in for a real path: keeps the library
    split (several summaries break down by it) without naming a title."""
    digest = hashlib.sha256(path.encode("utf-8")).hexdigest()[:8]
    return f"lib{lib_id}:{digest}"


def _on_sigint(signum, frame):
    global _stop
    _stop = True
    sys.stderr.write("\ninterrupt received; finishing current file then stopping\n")


def parse_map(specs):
    """--map /media=/Volumes/media  ->  [("/media", "/Volumes/media")]"""
    out = []
    for spec in specs or []:
        if "=" not in spec:
            sys.exit(f"--map needs FROM=TO, got: {spec}")
        frm, to = spec.split("=", 1)
        out.append((frm.rstrip("/"), to.rstrip("/")))
    # Longest prefix first so /media/Movies wins over /media.
    return sorted(out, key=lambda p: -len(p[0]))


def apply_map(path, mappings):
    for frm, to in mappings:
        if path == frm or path.startswith(frm + "/"):
            return to + path[len(frm):]
    return path


def paths_from_db(db_path, mappings):
    """media_items.path is relative to libraries.path."""
    con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rows = con.execute(
        "SELECT l.id, l.path, m.path FROM media_items m "
        "JOIN libraries l ON l.id = m.library_id ORDER BY l.id, m.path"
    ).fetchall()
    con.close()
    out = []
    for lib_id, lib_path, rel in rows:
        out.append((lib_id, apply_map(os.path.join(lib_path, rel), mappings)))
    return out


def paths_from_walk(roots):
    out = []
    for i, root in enumerate(roots, start=1):
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames.sort()
            for name in sorted(filenames):
                if os.path.splitext(name)[1].lower() in MEDIA_EXTS:
                    out.append((i, os.path.join(dirpath, name)))
    return out


def stratified_sample(items, n, rng):
    """Proportional across libraries, so a 500-row run reflects the real mix."""
    if n <= 0 or n >= len(items):
        return items
    by_lib = {}
    for lib_id, path in items:
        by_lib.setdefault(lib_id, []).append((lib_id, path))
    total = len(items)
    picked = []
    for lib_id, group in sorted(by_lib.items()):
        take = max(1, round(n * len(group) / total))
        picked.extend(rng.sample(group, min(take, len(group))))
    rng.shuffle(picked)
    return picked[:n]


def find_sidecars(path):
    stem = os.path.splitext(os.path.basename(path))[0]
    d = os.path.dirname(path)
    found = []
    try:
        for name in os.listdir(d):
            base, ext = os.path.splitext(name)
            if ext.lower() in SIDECAR_EXTS and base.startswith(stem):
                found.append(ext.lower().lstrip("."))
    except OSError:
        return ["?"]
    return sorted(set(found))


def probe(path, timeout):
    cmd = [
        "ffprobe", "-v", "error", "-select_streams", "s",
        "-show_entries",
        "format=format_name:stream=index,codec_name:"
        "stream_tags=language,title:"
        "stream_disposition=default,forced,hearing_impaired",
        "-of", "json", path,
    ]
    t0 = time.monotonic()
    try:
        proc = subprocess.run(cmd, capture_output=True, timeout=timeout, text=True)
    except subprocess.TimeoutExpired:
        return None, int((time.monotonic() - t0) * 1000), f"timeout after {timeout}s"
    except FileNotFoundError:
        sys.exit("ffprobe not found on PATH")
    ms = int((time.monotonic() - t0) * 1000)
    if proc.returncode != 0:
        return None, ms, (proc.stderr or "").strip().replace("\n", " ")[:200]
    try:
        return json.loads(proc.stdout or "{}"), ms, ""
    except json.JSONDecodeError as exc:
        return None, ms, f"unparseable ffprobe output: {exc}"


def classify(n_text, n_ass, n_image, sidecars):
    """Mirrors the intended status derivation. Order matters."""
    if n_text or n_ass:
        return "extract"          # embedded text/ASS -> full pass required
    if sidecars:
        return "sidecar_only"     # convert in process, no source read
    if n_image:
        return "image_only"       # burn-in owns it; no extract, no file
    return "no_subs"              # nothing at all


def summarise(csv_path):
    counts, text_items, probe_times, errors = {}, 0, [], 0
    with open(csv_path, newline="") as fh:
        for row in csv.DictReader(fh):
            if row["ok"] != "1":
                errors += 1
                continue
            counts[row["classification"]] = counts.get(row["classification"], 0) + 1
            if int(row["n_text"] or 0) or int(row["n_ass"] or 0):
                text_items += 1
            if row["probe_ms"]:
                probe_times.append(int(row["probe_ms"]))
    total = sum(counts.values())
    print("\n--- summary ---")
    print(f"probed ok: {total}    failed: {errors}")
    for key in ("extract", "sidecar_only", "image_only", "no_subs"):
        n = counts.get(key, 0)
        pct = (100.0 * n / total) if total else 0.0
        print(f"  {key:<14} {n:>7}  {pct:5.1f}%")
    if total:
        print(f"\nneed a full-file pass: {text_items} of {total} "
              f"({100.0 * text_items / total:.1f}%)")
    if probe_times:
        probe_times.sort()
        mid = probe_times[len(probe_times) // 2]
        p95 = probe_times[int(len(probe_times) * 0.95)]
        print(f"probe ms: median {mid}  p95 {p95}  max {probe_times[-1]}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    src = ap.add_argument_group("source (pick one)")
    src.add_argument("--db", help="read-only copy of nightjar.db; paths come from media_items")
    src.add_argument("--walk", action="append", metavar="DIR",
                     help="walk this directory for media files; repeatable")
    ap.add_argument("--map", action="append", metavar="FROM=TO",
                    help="rewrite a DB path prefix, e.g. /media=/Volumes/media; repeatable")
    ap.add_argument("--out", required=True, help="CSV output; appended and resumed")
    ap.add_argument("--sample", type=int, default=0,
                    help="probe only N files, proportional across libraries (0 = all)")
    ap.add_argument("--timeout", type=int, default=20,
                    help="per-file ffprobe timeout in seconds (default 20)")
    ap.add_argument("--seed", type=int, default=20260806, help="sample seed")
    ap.add_argument("--hash-paths", action="store_true",
                    help="write a stable lib{N}:{hash8} stand-in for `path` "
                         "instead of the real path; the file itself is still "
                         "read under the real path, only the CSV changes")
    ap.add_argument("--summary-only", action="store_true",
                    help="re-print the summary from an existing CSV and exit")
    args = ap.parse_args()

    if args.summary_only:
        summarise(args.out)
        return
    if bool(args.db) == bool(args.walk):
        ap.error("give exactly one of --db or --walk")

    mappings = parse_map(args.map)
    items = paths_from_db(args.db, mappings) if args.db else paths_from_walk(args.walk)
    print(f"{len(items)} files in source")

    if args.sample:
        items = stratified_sample(items, args.sample, random.Random(args.seed))
        print(f"sampled down to {len(items)}")

    done = set()
    if os.path.exists(args.out):
        with open(args.out, newline="") as fh:
            done = {r["path"] for r in csv.DictReader(fh)}
        print(f"{len(done)} already in {args.out}; skipping those")

    def key(lib, p):
        return hash_path(p, lib) if args.hash_paths else p

    todo = [(lib, p) for lib, p in items if key(lib, p) not in done]
    if not todo:
        print("nothing left to probe")
        summarise(args.out)
        return

    signal.signal(signal.SIGINT, _on_sigint)
    new_file = not os.path.exists(args.out)
    t_start = time.monotonic()

    with open(args.out, "a", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=FIELDS)
        if new_file:
            writer.writeheader()

        for i, (lib, path) in enumerate(todo, start=1):
            if _stop:
                break
            try:
                size_gb = round(os.path.getsize(path) / 1073741824.0, 2)
            except OSError:
                size_gb = ""

            data, ms, err = probe(path, args.timeout)
            row = {f: "" for f in FIELDS}
            out_path = hash_path(path, lib) if args.hash_paths else path
            row.update(path=out_path, probe_ms=ms, size_gb=size_gb)

            if data is None:
                row.update(ok="0", error=err)
            else:
                sidecars = find_sidecars(path)
                n_text = n_ass = n_image = n_unknown = 0
                parts = []
                for st in data.get("streams", []):
                    codec = (st.get("codec_name") or "?").lower()
                    if codec in TEXT_CODECS:
                        n_text += 1
                    elif codec in ASS_CODECS:
                        n_ass += 1
                    elif codec in IMAGE_CODECS:
                        n_image += 1
                    else:
                        n_unknown += 1
                    tags = st.get("tags") or {}
                    disp = st.get("disposition") or {}
                    flags = "".join(k[0] for k in ("default", "forced", "hearing_impaired")
                                    if disp.get(k))
                    parts.append(f"{st.get('index')}:{codec}:"
                                 f"{tags.get('language', '')}:{flags}")
                row.update(
                    ok="1",
                    container=(data.get("format") or {}).get("format_name", ""),
                    n_text=n_text, n_ass=n_ass, n_image=n_image, n_unknown=n_unknown,
                    sidecars="|".join(sidecars),
                    classification=classify(n_text, n_ass, n_image, sidecars),
                    streams="|".join(parts),
                )

            if args.hash_paths:
                # ffprobe/OS error text can embed the real path (e.g.
                # "No such file: '/Volumes/...'"); scrub any leftover copy
                # of it out of every column, not just `path`.
                for k, v in row.items():
                    if isinstance(v, str) and path in v:
                        row[k] = v.replace(path, out_path)

            writer.writerow(row)
            fh.flush()

            if i % 25 == 0 or i == len(todo):
                rate = i / max(time.monotonic() - t_start, 0.001)
                left = (len(todo) - i) / rate if rate else 0
                print(f"  {i}/{len(todo)}  {rate:.1f}/s  ~{left/60:.0f} min left",
                      flush=True)

    summarise(args.out)


if __name__ == "__main__":
    main()
