#!/usr/bin/env python3
"""Measure the cost of building a keyframe map from the container's own index.

For each file: locate and parse the container index (Matroska Cues, MP4 sync
sample tables), timing it and counting bytes actually read. Reports whether the
index exists, whether it covers the title, how many entries it holds, and how
long it took over this transport.

This is the ADR-0023 §2 "index-first" path measured on the real library. The
packet-walk fallback is only timed when --fallback is passed, because it demuxes
the whole file.

Read-only. Nothing is written except the CSV.

    python3 keyframe_index_probe.py \
        --db ~/nightjar.db.copy --map "/media=/Volumes/media" \
        --sample 300 --out keyframe-index-2026-08-06.csv

Add --fallback to also time an ffprobe packet walk. Use it on a small sample.
"""

import argparse
import csv
import hashlib
import io
import json
import os
import random
import signal
import sqlite3
import struct
import subprocess
import sys
import time

FIELDS = [
    "path", "container", "ok", "index_source", "n_entries",
    "first_pts_ms", "last_pts_ms", "duration_ms", "coverage_pct",
    "bytes_read", "n_reads", "index_ms", "moov_position",
    "size_gb", "fallback_ms", "fallback_entries", "error",
]

MEDIA_EXTS = {".mkv", ".mp4", ".m4v", ".mov", ".webm"}
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


READ_BUFFER = 256 * 1024


class _CountingRaw(io.RawIOBase):
    """Counts reads *below* the buffering layer, so `bytes_read` and `n_reads`
    are transport round trips rather than parser calls. A byte-at-a-time EBML
    parser over a buffered reader issues one network read per buffer, and that
    is the number worth measuring."""

    def __init__(self, path):
        self._f = open(path, "rb", buffering=0)
        self.bytes_read = 0
        self.n_reads = 0

    def readable(self):
        return True

    def seekable(self):
        return True

    def readinto(self, b):
        n = self._f.readinto(b)
        if n:
            self.bytes_read += n
            self.n_reads += 1
        return n

    def seek(self, pos, whence=0):
        return self._f.seek(pos, whence)

    def tell(self):
        return self._f.tell()

    def close(self):
        self._f.close()
        super().close()


class CountingReader:
    """Buffered reader over `_CountingRaw`, exposing the seek/read/tell surface
    the parsers use."""

    def __init__(self, path):
        self.raw = _CountingRaw(path)
        self.fh = io.BufferedReader(self.raw, buffer_size=READ_BUFFER)

    @property
    def bytes_read(self):
        return self.raw.bytes_read

    @property
    def n_reads(self):
        return self.raw.n_reads

    def seek(self, pos, whence=0):
        return self.fh.seek(pos, whence)

    def tell(self):
        return self.fh.tell()

    def read(self, n=-1):
        return self.fh.read(n)

    def size(self):
        cur = self.fh.tell()
        end = self.fh.seek(0, io.SEEK_END)
        self.fh.seek(cur)
        return end

    def close(self):
        self.fh.close()


# ----------------------------------------------------------------- Matroska

EBML_IDS = {
    "Segment": 0x18538067,
    "SeekHead": 0x114D9B74,
    "Seek": 0x4DBB,
    "SeekID": 0x53AB,
    "SeekPosition": 0x53AC,
    "Info": 0x1549A966,
    "TimestampScale": 0x2AD7B1,
    "Duration": 0x4489,
    "Cues": 0x1C53BB6B,
    "CuePoint": 0xBB,
    "CueTime": 0xB3,
    "CueTrackPositions": 0xB7,
    "CueClusterPosition": 0xF1,
    "Cluster": 0x1F43B675,
    "Void": 0xEC,
    "CRC32": 0xBF,
}
UNKNOWN_SIZE = object()


def read_vint(r, keep_marker):
    first = r.read(1)
    if not first:
        return None, 0
    b = first[0]
    if b == 0:
        raise ValueError("invalid EBML vint")
    length = 8 - b.bit_length() + 1
    rest = r.read(length - 1) if length > 1 else b""
    if len(rest) != length - 1:
        return None, 0
    raw = int.from_bytes(first + rest, "big")
    if keep_marker:
        return raw, length
    value = raw & ~(1 << (7 * length))
    if value == (1 << (7 * length)) - 1:
        return UNKNOWN_SIZE, length
    return value, length


def read_element_header(r):
    eid, n1 = read_vint(r, keep_marker=True)
    if eid is None:
        return None, None
    size, n2 = read_vint(r, keep_marker=False)
    if size is None:
        return None, None
    return eid, size


def read_uint(r, size):
    return int.from_bytes(r.read(size), "big") if size else 0


def read_float(r, size):
    data = r.read(size)
    if size == 4:
        return struct.unpack(">f", data)[0]
    if size == 8:
        return struct.unpack(">d", data)[0]
    return 0.0


def mkv_children(r, end):
    """Yield (id, size, body_start) for children up to `end`, skipping bodies."""
    while r.tell() < end:
        start = r.tell()
        eid, size = read_element_header(r)
        if eid is None:
            return
        body = r.tell()
        if size is UNKNOWN_SIZE:
            yield eid, None, body
            return
        yield eid, size, body
        nxt = body + size
        if nxt <= start:
            return
        r.seek(nxt)


def mkv_parse(r):
    """Cues via SeekHead where possible; top-level walk only as a fallback."""
    file_size = r.size()
    r.seek(0)
    eid, size = read_element_header(r)          # EBML header
    if eid != 0x1A45DFA3:
        raise ValueError("not EBML")
    r.seek(r.tell() + size)

    eid, size = read_element_header(r)
    if eid != EBML_IDS["Segment"]:
        raise ValueError("no Segment")
    seg_start = r.tell()
    seg_end = file_size if size is UNKNOWN_SIZE else seg_start + size

    timescale, duration_raw = 1_000_000, None
    cues_pos, source = None, None

    # SeekHead is at the head of the Segment on any sane muxer.
    for cid, csize, cbody in mkv_children(r, min(seg_start + 4096, seg_end)):
        if cid != EBML_IDS["SeekHead"] or csize is None:
            continue
        for sid, ssize, sbody in mkv_children(r, cbody + csize):
            if sid != EBML_IDS["Seek"] or ssize is None:
                continue
            want_id, want_pos = None, None
            for kid, ksize, kbody in mkv_children(r, sbody + ssize):
                if ksize is None:
                    continue
                r.seek(kbody)
                if kid == EBML_IDS["SeekID"]:
                    want_id = int.from_bytes(r.read(ksize), "big")
                elif kid == EBML_IDS["SeekPosition"]:
                    want_pos = read_uint(r, ksize)
            if want_id == EBML_IDS["Cues"] and want_pos is not None:
                cues_pos, source = seg_start + want_pos, "seekhead"
        break

    # Info, for timescale and duration.
    r.seek(seg_start)
    for cid, csize, cbody in mkv_children(r, seg_end):
        if csize is None:
            break
        if cid == EBML_IDS["Info"]:
            for iid, isize, ibody in mkv_children(r, cbody + csize):
                if isize is None:
                    continue
                r.seek(ibody)
                if iid == EBML_IDS["TimestampScale"]:
                    timescale = read_uint(r, isize) or timescale
                elif iid == EBML_IDS["Duration"]:
                    duration_raw = read_float(r, isize)
        elif cid == EBML_IDS["Cues"] and cues_pos is None:
            cues_pos, source = cbody, "toplevel"
        elif cid == EBML_IDS["Cluster"] and cues_pos is not None and duration_raw:
            break

    duration_ms = int(duration_raw * timescale / 1e6) if duration_raw else None
    if cues_pos is None:
        return {"index_source": "absent", "entries": [], "duration_ms": duration_ms}

    r.seek(cues_pos)
    eid, size = read_element_header(r)
    if eid != EBML_IDS["Cues"] or size is UNKNOWN_SIZE:
        return {"index_source": "absent", "entries": [], "duration_ms": duration_ms}

    entries, cues_end = [], r.tell() + size
    for cid, csize, cbody in mkv_children(r, cues_end):
        if cid != EBML_IDS["CuePoint"] or csize is None:
            continue
        cue_time, cluster_pos = None, None
        for pid, psize, pbody in mkv_children(r, cbody + csize):
            if psize is None:
                continue
            if pid == EBML_IDS["CueTime"]:
                r.seek(pbody)
                cue_time = read_uint(r, psize)
            elif pid == EBML_IDS["CueTrackPositions"]:
                for tid, tsize, tbody in mkv_children(r, pbody + psize):
                    if tid == EBML_IDS["CueClusterPosition"] and tsize:
                        r.seek(tbody)
                        cluster_pos = seg_start + read_uint(r, tsize)
        if cue_time is not None and cluster_pos is not None:
            entries.append((int(cue_time * timescale / 1e6), cluster_pos))
    entries.sort()
    return {"index_source": source, "entries": entries, "duration_ms": duration_ms}


# ---------------------------------------------------------------------- MP4

def mp4_boxes(r, end):
    while r.tell() < end - 8:
        start = r.tell()
        head = r.read(8)
        if len(head) < 8:
            return
        size = int.from_bytes(head[0:4], "big")
        btype = head[4:8].decode("latin-1")
        if size == 1:
            size = int.from_bytes(r.read(8), "big")
            body = start + 16
        elif size == 0:
            size, body = end - start, start + 8
        else:
            body = start + 8
        if size < 8:
            return
        yield btype, start, body, start + size
        r.seek(start + size)


def mp4_table(buf, offset):
    """Returns (entry_count, data_offset) for a full box with version/flags."""
    count = int.from_bytes(buf[offset + 4:offset + 8], "big")
    return count, offset + 8


def mp4_parse(r):
    file_size = r.size()
    r.seek(0)
    moov = None
    moov_position = "absent"
    first_mdat = None
    for btype, start, body, end in mp4_boxes(r, file_size):
        if btype == "moov":
            moov = (body, end)
            moov_position = "faststart" if first_mdat is None else "end"
        elif btype == "mdat" and first_mdat is None:
            first_mdat = start
        if moov and first_mdat is not None:
            break
    if not moov:
        return {"index_source": "absent", "entries": [], "duration_ms": None,
                "moov_position": moov_position}

    body, end = moov
    r.seek(body)
    moov_buf = r.read(end - body)          # one read; this is the index cost
    base = body

    duration_ms = None
    result = None
    for btype, start, tbody, tend in mp4_boxes(_Buf(moov_buf, base), end):
        if btype == "mvhd":
            off = tbody - base
            ver = moov_buf[off]
            ts_off = off + 4 + (16 if ver == 1 else 8)
            timescale = int.from_bytes(moov_buf[ts_off:ts_off + 4], "big")
            dur = int.from_bytes(
                moov_buf[ts_off + 4: ts_off + 4 + (8 if ver == 1 else 4)], "big")
            if timescale:
                duration_ms = int(dur * 1000 / timescale)
        elif btype == "trak":
            got = _mp4_trak(moov_buf, base, tbody, tend)
            if got:
                result = got
    if not result:
        return {"index_source": "absent", "entries": [], "duration_ms": duration_ms,
                "moov_position": moov_position}
    result.update(duration_ms=duration_ms, moov_position=moov_position)
    return result


class _Buf:
    """Read-only view of an in-memory buffer addressed by absolute file offset,
    so the box walker can be reused without further I/O."""

    def __init__(self, buf, base):
        self.buf, self.base, self.pos = buf, base, base

    def read(self, n=-1):
        i = self.pos - self.base
        data = self.buf[i:] if n < 0 else self.buf[i:i + n]
        self.pos += len(data)
        return data

    def seek(self, pos, whence=0):
        self.pos = pos if whence == 0 else self.pos + pos
        return self.pos

    def tell(self):
        return self.pos


def _find(buf, base, body, end, want):
    view = _Buf(buf, base)
    view.seek(body)
    for btype, start, tbody, tend in mp4_boxes(view, end):
        if btype == want:
            return tbody, tend
        if btype in ("mdia", "minf", "stbl"):
            got = _find(buf, base, tbody, tend, want)
            if got:
                return got
    return None


def _mp4_trak(buf, base, body, end):
    """Video track sample tables -> keyframe (pts_ms, byte_offset) entries."""
    hdlr = _find(buf, base, body, end, "hdlr")
    if not hdlr:
        return None
    o = hdlr[0] - base
    if buf[o + 8:o + 12] != b"vide":
        return None

    mdhd = _find(buf, base, body, end, "mdhd")
    timescale = 0
    if mdhd:
        o = mdhd[0] - base
        ver = buf[o]
        ts_off = o + 4 + (16 if ver == 1 else 8)
        timescale = int.from_bytes(buf[ts_off:ts_off + 4], "big")
    if not timescale:
        return None

    def table(name):
        got = _find(buf, base, body, end, name)
        return (got[0] - base) if got else None

    stss_o, stts_o = table("stss"), table("stts")
    stsc_o, stsz_o = table("stsc"), table("stsz")
    stco_o, co64_o = table("stco"), table("co64")
    if stss_o is None or stts_o is None or stsc_o is None:
        return None
    if stco_o is None and co64_o is None:
        return None

    n_stss, d = mp4_table(buf, stss_o)
    sync = [int.from_bytes(buf[d + 4 * i: d + 4 * i + 4], "big") for i in range(n_stss)]
    if not sync:
        return {"index_source": "stss-empty", "entries": []}

    n_stts, d = mp4_table(buf, stts_o)
    deltas = [(int.from_bytes(buf[d + 8 * i: d + 8 * i + 4], "big"),
               int.from_bytes(buf[d + 8 * i + 4: d + 8 * i + 8], "big"))
              for i in range(n_stts)]

    n_stsc, d = mp4_table(buf, stsc_o)
    chunks = [(int.from_bytes(buf[d + 12 * i: d + 12 * i + 4], "big"),
               int.from_bytes(buf[d + 12 * i + 4: d + 12 * i + 8], "big"))
              for i in range(n_stsc)]

    if stsz_o is not None:
        uniform = int.from_bytes(buf[stsz_o + 4:stsz_o + 8], "big")
        n_sz, d = int.from_bytes(buf[stsz_o + 8:stsz_o + 12], "big"), stsz_o + 12
        sizes = None if uniform else [
            int.from_bytes(buf[d + 4 * i: d + 4 * i + 4], "big") for i in range(n_sz)]
    else:
        uniform, sizes = 0, None

    if co64_o is not None:
        n_co, d = mp4_table(buf, co64_o)
        offsets = [int.from_bytes(buf[d + 8 * i: d + 8 * i + 8], "big") for i in range(n_co)]
    else:
        n_co, d = mp4_table(buf, stco_o)
        offsets = [int.from_bytes(buf[d + 4 * i: d + 4 * i + 4], "big") for i in range(n_co)]

    # sample number (1-based) -> decode time
    times, t = {}, 0
    n = 1
    wanted = set(sync)
    for count, delta in deltas:
        for _ in range(count):
            if n in wanted:
                times[n] = t
            t += delta
            n += 1

    # sample number -> (chunk index, index within chunk)
    entries = []
    for s in sync:
        chunk, first_sample = 1, 1
        for i, (first_chunk, per_chunk) in enumerate(chunks):
            next_first = chunks[i + 1][0] if i + 1 < len(chunks) else None
            n_chunks = (next_first - first_chunk) if next_first else None
            span = (n_chunks * per_chunk) if n_chunks else None
            if span is not None and s >= first_sample + span:
                first_sample += span
                chunk = next_first
                continue
            within = (s - first_sample) // per_chunk
            chunk = first_chunk + within
            idx_in_chunk = (s - first_sample) % per_chunk
            if chunk - 1 >= len(offsets):
                break
            off = offsets[chunk - 1]
            if uniform:
                off += uniform * idx_in_chunk
            elif sizes:
                start_sample = s - idx_in_chunk
                off += sum(sizes[start_sample - 1: s - 1])
            entries.append((int(times.get(s, 0) * 1000 / timescale), off))
            break
    entries.sort()
    return {"index_source": "sample-tables", "entries": entries}


# ----------------------------------------------------------------- fallback

def packet_walk(path, timeout):
    cmd = ["ffprobe", "-v", "error", "-select_streams", "v:0", "-skip_frame",
           "nokey", "-show_entries", "frame=pts_time,pkt_pos", "-of", "csv=p=0", path]
    t0 = time.monotonic()
    try:
        proc = subprocess.run(cmd, capture_output=True, timeout=timeout, text=True)
    except subprocess.TimeoutExpired:
        return int((time.monotonic() - t0) * 1000), -1
    ms = int((time.monotonic() - t0) * 1000)
    if proc.returncode != 0:
        return ms, -1
    return ms, sum(1 for line in proc.stdout.splitlines() if line.strip())


# --------------------------------------------------------------------- main

def detect(path):
    with open(path, "rb") as fh:
        head = fh.read(12)
    if head[:4] == b"\x1a\x45\xdf\xa3":
        return "matroska"
    if head[4:8] in (b"ftyp", b"moov", b"mdat", b"free"):
        return "mp4"
    return "other"


def probe_one(path, args, lib=None):
    out_path = hash_path(path, lib) if args.hash_paths else path

    def done(row):
        if args.hash_paths:
            # ffprobe/OS error text can embed the real path (e.g.
            # "No such file: '/Volumes/...'"); scrub any leftover copy of
            # it out of every column, not just `path`.
            for k, v in row.items():
                if isinstance(v, str) and path in v:
                    row[k] = v.replace(path, out_path)
        row["path"] = out_path
        return row

    kind = detect(path)
    row = {f: "" for f in FIELDS}
    row.update(path=out_path, container=kind)
    try:
        row["size_gb"] = round(os.path.getsize(path) / 1073741824.0, 2)
    except OSError:
        pass
    if kind == "other":
        row.update(ok="0", error="unrecognised container")
        return done(row)

    t0 = time.monotonic()
    r = None
    try:
        r = CountingReader(path)
        got = mkv_parse(r) if kind == "matroska" else mp4_parse(r)
        ms = int((time.monotonic() - t0) * 1000)
        bytes_read, n_reads = r.bytes_read, r.n_reads
        r.close()
    except Exception as exc:                    # parse or I/O
        row.update(ok="0", index_ms=int((time.monotonic() - t0) * 1000),
                   bytes_read=(r.bytes_read if r else ""),
                   n_reads=(r.n_reads if r else ""),
                   error=f"{type(exc).__name__}: {exc}"[:180])
        if r:
            try:
                r.close()
            except OSError:
                pass
        return done(row)

    entries = got.get("entries") or []
    dur = got.get("duration_ms")
    last = entries[-1][0] if entries else None
    row.update(
        ok="1", index_source=got.get("index_source", ""), n_entries=len(entries),
        first_pts_ms=(entries[0][0] if entries else ""), last_pts_ms=(last or ""),
        duration_ms=(dur or ""), bytes_read=bytes_read, n_reads=n_reads,
        index_ms=ms, moov_position=got.get("moov_position", ""),
        coverage_pct=(round(100.0 * last / dur, 1) if (last and dur) else ""),
    )
    if args.fallback:
        fms, fn = packet_walk(path, args.fallback_timeout)
        row.update(fallback_ms=fms, fallback_entries=fn)
    return done(row)


def paths_from_db(db, mappings):
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = con.execute(
        "SELECT l.id, l.path, m.path FROM media_items m "
        "JOIN libraries l ON l.id = m.library_id ORDER BY l.id, m.path").fetchall()
    con.close()
    out = []
    for lib, lib_path, rel in rows:
        p = os.path.join(lib_path, rel)
        for frm, to in mappings:
            if p == frm or p.startswith(frm + "/"):
                p = to + p[len(frm):]
                break
        out.append((lib, p))
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


def stratified(items, n, rng):
    if n <= 0 or n >= len(items):
        return items
    groups = {}
    for lib, p in items:
        groups.setdefault(lib, []).append((lib, p))
    picked = []
    for lib, g in sorted(groups.items()):
        take = max(1, round(n * len(g) / len(items)))
        picked.extend(rng.sample(g, min(take, len(g))))
    rng.shuffle(picked)
    return picked[:n]


def summarise(out):
    rows = list(csv.DictReader(open(out, newline="")))
    ok = [r for r in rows if r["ok"] == "1"]
    print("\n--- summary ---")
    print(f"probed {len(rows)}   ok {len(ok)}   failed {len(rows) - len(ok)}")
    for kind in sorted({r["container"] for r in ok}):
        sub = [r for r in ok if r["container"] == kind]
        have = [r for r in sub if int(r["n_entries"] or 0) > 0]
        print(f"\n{kind}: {len(sub)} files, index usable on {len(have)} "
              f"({100.0 * len(have) / max(len(sub), 1):.1f}%)")
        src = {}
        for r in sub:
            src[r["index_source"]] = src.get(r["index_source"], 0) + 1
        print("  index source:", ", ".join(f"{k}={v}" for k, v in sorted(src.items())))
        if have:
            for col, label in (("index_ms", "index ms"), ("bytes_read", "bytes read"),
                               ("n_reads", "reads"), ("n_entries", "entries")):
                vals = sorted(int(r[col]) for r in have if r[col] != "")
                if vals:
                    mid = vals[len(vals) // 2]
                    p95 = vals[min(int(len(vals) * 0.95), len(vals) - 1)]
                    print(f"  {label:<11} median {mid:>10,}  p95 {p95:>10,}  max {vals[-1]:>10,}")
            cov = [float(r["coverage_pct"]) for r in have if r["coverage_pct"] != ""]
            if cov:
                short = [c for c in cov if c < 98.0]
                print(f"  coverage    median {sorted(cov)[len(cov)//2]:.1f}%   "
                      f"under 98%: {len(short)} of {len(cov)}")
            if kind == "mp4":
                mp = {}
                for r in sub:
                    mp[r["moov_position"]] = mp.get(r["moov_position"], 0) + 1
                print("  moov:", ", ".join(f"{k}={v}" for k, v in sorted(mp.items())))
    fb = [r for r in ok if r["fallback_ms"] not in ("", None)]
    if fb:
        vals = sorted(int(r["fallback_ms"]) for r in fb)
        print(f"\npacket walk (n={len(fb)}): median {vals[len(vals)//2]:,} ms  "
              f"max {vals[-1]:,} ms")
        pairs = [(int(r["index_ms"]), int(r["fallback_ms"])) for r in fb
                 if r["index_ms"] != "" and int(r["fallback_ms"]) > 0]
        if pairs:
            ratio = sorted(f / max(i, 1) for i, f in pairs)
            print(f"  fallback / index cost ratio: median {ratio[len(ratio)//2]:.0f}x")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db")
    ap.add_argument("--walk", action="append", metavar="DIR")
    ap.add_argument("--map", action="append", metavar="FROM=TO", default=[])
    ap.add_argument("--out", required=True)
    ap.add_argument("--sample", type=int, default=0)
    ap.add_argument("--seed", type=int, default=20260806)
    ap.add_argument("--fallback", action="store_true",
                    help="also time an ffprobe packet walk (slow; small samples only)")
    ap.add_argument("--fallback-timeout", type=int, default=900)
    ap.add_argument("--hash-paths", action="store_true",
                    help="write a stable lib{N}:{hash8} stand-in for `path` "
                         "instead of the real path; the file itself is still "
                         "read under the real path, only the CSV changes")
    ap.add_argument("--summary-only", action="store_true")
    args = ap.parse_args()

    if args.summary_only:
        summarise(args.out)
        return
    if bool(args.db) == bool(args.walk):
        ap.error("give exactly one of --db or --walk")

    mappings = sorted(
        ((s.split("=", 1)[0].rstrip("/"), s.split("=", 1)[1].rstrip("/"))
         for s in args.map), key=lambda p: -len(p[0]))
    items = paths_from_db(args.db, mappings) if args.db else paths_from_walk(args.walk)
    print(f"{len(items)} files in source")
    if args.sample:
        items = stratified(items, args.sample, random.Random(args.seed))
        print(f"sampled down to {len(items)}")

    done = set()
    if os.path.exists(args.out):
        done = {r["path"] for r in csv.DictReader(open(args.out, newline=""))}
        print(f"{len(done)} already done; skipping")

    def key(lib, p):
        return hash_path(p, lib) if args.hash_paths else p

    todo = [(l, p) for l, p in items if key(l, p) not in done]
    if not todo:
        summarise(args.out)
        return

    signal.signal(signal.SIGINT, _on_sigint)
    new = not os.path.exists(args.out)
    t0 = time.monotonic()
    with open(args.out, "a", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=FIELDS)
        if new:
            w.writeheader()
        for i, (lib, path) in enumerate(todo, start=1):
            if _stop:
                break
            w.writerow(probe_one(path, args, lib))
            fh.flush()
            if i % 25 == 0 or i == len(todo):
                rate = i / max(time.monotonic() - t0, 1e-3)
                print(f"  {i}/{len(todo)}  {rate:.1f}/s  "
                      f"~{(len(todo) - i) / rate / 60:.0f} min left", flush=True)
    summarise(args.out)


if __name__ == "__main__":
    main()
