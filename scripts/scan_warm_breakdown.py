#!/usr/bin/env python3
"""Warm-walk breakdown measure for scanner residual E0.

Times a directory-mtime-cached media walk (ADR-0013 shape), then the
canonicalize + fold-match legs that the product index applies per file.

Usage (on N150 host):
  python3 scripts/scan_warm_breakdown.py \\
    --root /mnt/media/Movies \\
    --db ~/gate2/data-docker-dogfood/nightjar.db \\
    --library-id 1

  python3 scripts/scan_warm_breakdown.py \\
    --root /mnt/media/TV\\ Shows \\
    --db ~/gate2/data-docker-dogfood/nightjar.db \\
    --library-id 2

Does not modify the DB. Product concurrency is not simulated (serial walk);
NIGHTJAR_WALK_CONCURRENCY on the box is typically 8, so wall walk is lower.
"""

from __future__ import annotations

import argparse
import os
import sqlite3
import sys
import time
from pathlib import Path

MEDIA_EXTS = {
    "mp4",
    "m4v",
    "mkv",
    "avi",
    "mov",
    "webm",
    "ts",
    "m2ts",
    "wmv",
    "mpg",
    "mpeg",
    "ogv",
}


def is_media(p: Path) -> bool:
    return p.suffix.lstrip(".").lower() in MEDIA_EXTS


def fold_path(s: str) -> str:
    return "/".join(seg.lower() for seg in s.replace("\\", "/").split("/"))


def mtime_ms(st: os.stat_result) -> int:
    return int(st.st_mtime * 1000)


class CachedDir:
    __slots__ = ("mtime_ms", "files", "children")

    def __init__(self, mtime_ms: int, files: list, children: list[Path]):
        self.mtime_ms = mtime_ms
        self.files = files
        self.children = children


def walk_cached(root: Path, cache: dict[Path, CachedDir]) -> tuple[list[Path], int, int, int]:
    """Return (files, dirs_visited, dirs_relisted, listing_errors)."""
    files_out: list[Path] = []
    visited = 0
    relisted = 0
    errors = 0
    stack = [root.resolve()]
    seen: set[Path] = set()
    next_cache: dict[Path, CachedDir] = {}

    while stack:
        d = stack.pop()
        try:
            canon = d.resolve()
        except OSError:
            errors += 1
            continue
        if canon in seen:
            continue
        seen.add(canon)
        visited += 1
        try:
            st = d.stat()
        except OSError:
            errors += 1
            continue
        mt = mtime_ms(st)
        prev = cache.get(d)
        if prev is not None and prev.mtime_ms == mt:
            files_out.extend(prev.files)
            stack.extend(prev.children)
            next_cache[d] = prev
            continue
        relisted += 1
        children: list[Path] = []
        media_files: list[Path] = []
        try:
            with os.scandir(d) as it:
                for ent in it:
                    try:
                        p = Path(ent.path)
                        if ent.is_dir(follow_symlinks=False):
                            children.append(p)
                        elif ent.is_file(follow_symlinks=True) and is_media(p):
                            media_files.append(p)
                    except OSError:
                        errors += 1
        except OSError:
            errors += 1
            continue
        files_out.extend(media_files)
        stack.extend(children)
        next_cache[d] = CachedDir(mt, media_files, children)

    cache.clear()
    cache.update(next_cache)
    files_out.sort(key=lambda p: str(p))
    return files_out, visited, relisted, errors


def time_call(fn):
    t0 = time.perf_counter()
    out = fn()
    ms = (time.perf_counter() - t0) * 1000.0
    return out, ms


def load_db_folds(db_path: Path, library_id: int) -> dict[str, tuple[int, int]]:
    """fold -> (id, mtime_ms) for library items."""
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT id, path, mtime_ms FROM media_items WHERE library_id = ?",
        (library_id,),
    ).fetchall()
    conn.close()
    out: dict[str, tuple[int, int]] = {}
    for iid, path, mt in rows:
        out[fold_path(path)] = (iid, mt)
    return out


def to_relpath(root: Path, file: Path) -> str | None:
    try:
        rel = file.resolve().relative_to(root.resolve())
    except ValueError:
        return None
    s = rel.as_posix()
    if not s or s.startswith("..") or s == ".":
        return None
    return s


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", required=True, type=Path, help="library root to walk")
    ap.add_argument("--db", type=Path, help="optional nightjar.db for fold-match leg")
    ap.add_argument("--library-id", type=int, help="library_id when --db is set")
    ap.add_argument("--label", default="", help="label for the note (Movies / TV)")
    args = ap.parse_args()

    root = args.root
    if not root.is_dir():
        print(f"not a directory: {root}", file=sys.stderr)
        return 1

    label = args.label or root.name
    print(f"=== warm-walk breakdown: {label} ===")
    print(f"root={root}")
    print(f"host_time={time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}")

    cache: dict[Path, CachedDir] = {}

    (files1, vis1, rel1, err1), cold_ms = time_call(lambda: walk_cached(root, cache))
    print(
        f"cold_walk_ms={cold_ms:.1f} files={len(files1)} dirs_visited={vis1} "
        f"dirs_relisted={rel1} listing_errors={err1} cache_dirs={len(cache)}"
    )

    (files2, vis2, rel2, err2), warm_ms = time_call(lambda: walk_cached(root, cache))
    print(
        f"warm_walk_ms={warm_ms:.1f} files={len(files2)} dirs_visited={vis2} "
        f"dirs_relisted={rel2} listing_errors={err2} cache_dirs={len(cache)}"
    )

    def canon_all():
        ok = 0
        fail = 0
        for p in files2:
            try:
                p.resolve(strict=False)
                ok += 1
            except OSError:
                fail += 1
        return ok, fail

    (ok, fail), canon_ms = time_call(canon_all)
    print(f"canonicalize_all_ms={canon_ms:.1f} ok={ok} fail={fail}")

    # Product-ish: resolve + to_relpath + fold (no upsert).
    def resolve_rel_fold():
        n = 0
        outside = 0
        for p in files2:
            rel = to_relpath(root, p)
            if rel is None:
                outside += 1
                continue
            _ = fold_path(rel)
            n += 1
        return n, outside

    (n_rel, outside), rel_ms = time_call(resolve_rel_fold)
    print(f"resolve_relpath_fold_ms={rel_ms:.1f} ok={n_rel} outside={outside}")

    if args.db and args.library_id is not None:
        db_path = args.db.expanduser()
        folds, db_ms = time_call(lambda: load_db_folds(db_path, args.library_id))
        print(f"db_list_paths_ms={db_ms:.1f} rows={len(folds)}")

        def match_leg():
            unchanged = 0
            upsert = 0
            miss = 0
            for p in files2:
                rel = to_relpath(root, p)
                if rel is None:
                    continue
                try:
                    st = p.stat()
                    mt = mtime_ms(st)
                except OSError:
                    miss += 1
                    continue
                row = folds.get(fold_path(rel))
                if row is None:
                    upsert += 1
                elif row[1] == mt:
                    unchanged += 1
                else:
                    upsert += 1
            return unchanged, upsert, miss

        (unchanged, upsert, miss), match_ms = time_call(match_leg)
        print(
            f"fold_match_ms={match_ms:.1f} unchanged={unchanged} "
            f"would_upsert={upsert} stat_fail={miss}"
        )
        total_indexish = warm_ms + canon_ms + match_ms
        print(f"sum_warm_walk_plus_canon_plus_match_ms={total_indexish:.1f}")
        if total_indexish > 0:
            print(
                f"pct_walk={100*warm_ms/total_indexish:.1f} "
                f"pct_canon={100*canon_ms/total_indexish:.1f} "
                f"pct_match={100*match_ms/total_indexish:.1f}"
            )
    else:
        print("db_list_paths_ms=skipped (pass --db and --library-id)")

    print("note: walk is serial; product default walk concurrency is 8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
